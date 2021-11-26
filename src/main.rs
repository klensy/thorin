use crate::relocate::{add_relocations, Relocate, RelocationMap};
use anyhow::{anyhow, Context, Result};
use gimli::{
    write::{EndianVec, Writer},
    DebugStrOffset, DebugStrOffsetsBase, DebugStrOffsetsIndex, DwarfFileType, EndianSlice, Format,
    Reader, RunTimeEndian, UnitIndex, UnitType,
};
use indexmap::IndexSet;
use memmap2::Mmap;
use object::{
    write::{self, SectionId, StreamingBuffer},
    BinaryFormat, Endianness, Object, ObjectSection, SectionKind,
};
use std::borrow::{Borrow, Cow};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Stdout, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use thiserror::Error;
use tracing::{debug, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};
use tracing_tree::HierarchicalLayer;
use typed_arena::Arena;

mod relocate;

type DwpReader<'arena> = Relocate<'arena, EndianSlice<'arena, RunTimeEndian>>;

#[derive(Debug, Error)]
enum DwpError {
    #[error("compilation unit with dwo id has no dwo name")]
    DwoIdWithoutDwoName,
    #[error("missing compilation unit die")]
    MissingUnitDie,
    #[error("section without name at offset 0x{0:08x}")]
    SectionWithoutName(usize),
    #[error("relocation with invalid symbol for section {0} at offset 0x{1:08x}")]
    RelocationWithInvalidSymbol(String, usize),
    #[error("multiple relocations for section {0} at offset 0x{1:08x}")]
    MultipleRelocations(String, usize),
    #[error("unsupported relocation for section {0} at offset 0x{1:08x}")]
    UnsupportedRelocation(String, usize),
    #[error("missing {0} section in dwarf object")]
    DwarfObjectMissingSection(String),
    #[error("failed to create output file")]
    FailedToCreateOutputFile,
    #[error("dwarf object has no units")]
    DwarfObjectWithNoUnits,
    #[error("str offset value out of range of entry size")]
    DwpStrOffsetOutOfRange,
    #[error("compilation unit in dwarf object with dwo id is not a split unit")]
    DwarfObjectCompilationUnitWithDwoIdNotSplitUnit,
    #[error("compilation unit in dwarf object with no data")]
    CompilationUnitWithNoData,
    #[error("no data when reading header of DWARF 5 `.debug_str_offsets.dwo`")]
    Dwarf5StrOffsetWithoutHeader,
    #[error("unit(s) {0:?} was referenced by executable but not found")]
    MissingReferencedUnit(Vec<DwarfObjectIdentifier>),
}

/// DWARF packages come in pre-standard GNU extension format or DWARF 5 standardized format.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
enum PackageFormat {
    /// GNU's DWARF package file format (preceded standardized version from DWARF 5).
    ///
    /// See [specification](https://gcc.gnu.org/wiki/DebugFissionDWP).
    GnuExtension,
    /// DWARF 5-standardized package file format.
    ///
    /// See Sec 7.3.5 and Appendix F of [DWARF specification](https://dwarfstd.org/doc/DWARF5.pdf).
    DwarfStd,
}

impl PackageFormat {
    /// Returns the appropriate `PackageFormat` for the given version of DWARF being used.
    fn from_dwarf_version(version: u16) -> Self {
        if version >= 5 {
            PackageFormat::DwarfStd
        } else {
            PackageFormat::GnuExtension
        }
    }
}

impl fmt::Display for PackageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PackageFormat::GnuExtension => write!(f, "GNU Extension"),
            PackageFormat::DwarfStd => write!(f, "Dwarf Standard"),
        }
    }
}

impl Default for PackageFormat {
    fn default() -> Self {
        PackageFormat::GnuExtension
    }
}

/// Helper trait for types that have a corresponding `gimli::SectionId`.
trait HasGimliId {
    /// Return the corresponding `gimli::SectionId`.
    fn gimli_id() -> gimli::SectionId;
}

macro_rules! define_section_markers {
    ( $( $name:ident ),+ ) => {
        $(
            /// Marker type implementing `HasGimliId`, corresponds to `gimli::SectionId::$name`.
            /// Intended for use with `LazySectionId`.
            #[derive(Default)]
            struct $name;

            impl HasGimliId for $name {
                fn gimli_id() -> gimli::SectionId { gimli::SectionId::$name }
            }
        )+
    }
}

define_section_markers!(DebugInfo, DebugAbbrev, DebugStr, DebugTypes, DebugLine, DebugLoc);
define_section_markers!(DebugLocLists, DebugRngLists, DebugStrOffsets, DebugMacinfo, DebugMacro);
define_section_markers!(DebugCuIndex, DebugTuIndex);

/// Wrapper around `Option<SectionId>` for creating the `SectionId` on first access (if it does
/// not exist).
#[derive(Default)]
struct LazySectionId<Id: HasGimliId> {
    id: Option<SectionId>,
    _id: std::marker::PhantomData<Id>,
}

impl<Id: HasGimliId> LazySectionId<Id> {
    /// Return the `SectionId` for the current section, creating it if it does not exist.
    ///
    /// Don't call this function if the returned id isn't going to be used, otherwise an empty
    /// section would be created.
    fn get<'file>(&mut self, obj: &mut write::Object<'file>) -> SectionId {
        match self.id {
            Some(id) => id,
            None => {
                let id = obj.add_section(
                    Vec::new(),
                    dwo_name(Id::gimli_id()).as_bytes().to_vec(),
                    SectionKind::Debug,
                );
                self.id = Some(id);
                id
            }
        }
    }
}

/// Helper trait that abstracts over `gimli::DebugCuIndex` and `gimli::DebugTuIndex`.
trait IndexSection<'input, Endian: gimli::Endianity, R: gimli::Reader>: gimli::Section<R> {
    fn new(section: &'input [u8], endian: Endian) -> Self;

    fn index(self) -> gimli::read::Result<UnitIndex<R>>;
}

impl<'input, Endian: gimli::Endianity> IndexSection<'input, Endian, EndianSlice<'input, Endian>>
    for gimli::DebugCuIndex<EndianSlice<'input, Endian>>
{
    fn new(section: &'input [u8], endian: Endian) -> Self {
        Self::new(section, endian)
    }

    fn index(self) -> gimli::read::Result<UnitIndex<EndianSlice<'input, Endian>>> {
        Self::index(self)
    }
}

impl<'input, Endian: gimli::Endianity> IndexSection<'input, Endian, EndianSlice<'input, Endian>>
    for gimli::DebugTuIndex<EndianSlice<'input, Endian>>
{
    fn new(section: &'input [u8], endian: Endian) -> Self {
        Self::new(section, endian)
    }

    fn index(self) -> gimli::read::Result<UnitIndex<EndianSlice<'input, Endian>>> {
        Self::index(self)
    }
}

/// Returns the parsed unit index from a `.debug_{cu,tu}_index` section.
fn maybe_load_index_section<'input, 'arena: 'input, Endian, Index, R>(
    arena_compression: &'arena Arena<Vec<u8>>,
    endian: Endian,
    input: &object::File<'input>,
) -> Result<Option<UnitIndex<R>>>
where
    Endian: gimli::Endianity,
    Index: IndexSection<'input, Endian, R>,
    R: gimli::Reader,
{
    let index_name = Index::id().dwo_name().unwrap();
    if let Some(index_section) = input.section_by_name(index_name) {
        let index_data = index_section.compressed_data()?.decompress()?;
        let index_data_ref = match index_data {
            Cow::Borrowed(data) => data,
            Cow::Owned(data) => (*arena_compression.alloc(data)).borrow(),
        };
        let unit_index = Index::new(index_data_ref, endian).index()?;
        Ok(Some(unit_index))
    } else {
        Ok(None)
    }
}

/// Returns a closure which takes an identifier and a `Option<Contribution>`, and returns an
/// adjusted contribution if the input file is a DWARF package (and the contribution was
/// present).
///
/// For example, consider the `.debug_str_offsets` section: DWARF packages have a single
/// `.debug_str_offsets` section which contains the string offsets of all of its compilation/type
/// units, the contributions of each unit into that section are tracked in its
/// `.debug_{cu,tu}_index` section.
///
/// When a DWARF package is the input, the contributions of the units which constituted that
/// package should not be lost when its `.debug_str_offsets` section is merged with the new
/// DWARF package currently being created.
///
/// Given a parsed index section, use the size of its contribution to `.debug_str_offsets` as the
/// size of its contribution in the new unit (without this, it would be the size of the entire
/// `.debug_str_offsets` section from the input, rather than the part that the compilation unit
/// originally contributed to that). For subsequent units from the input, the offset in the
/// contribution will need to be adjusted to based on the size of the previous units.
///
/// This function returns a "contribution adjustor" closure, which adjusts the contribution's
/// offset and size according to its contribution in the input's index and with an offset
/// accumulated over all calls to the closure.
fn create_contribution_adjustor<'input, Identifier, Target, R: 'input>(
    index: Option<&'input UnitIndex<R>>,
) -> Result<Box<dyn FnMut(Identifier, Option<Contribution>) -> Result<Option<Contribution>> + 'input>>
where
    Identifier: Bucketable,
    Target: HasGimliId,
    R: gimli::Reader,
{
    let mut adjustment = 0;

    Ok(Box::new(
        move |identifier: Identifier,
              contribution: Option<Contribution>|
              -> Result<Option<Contribution>> {
            match (&index, contribution) {
                // dwp input with section
                (Some(index), Some(contribution)) => {
                    let row_id = index.find(identifier.index()).expect("dwp unit not in index");
                    let str_offset_section = index
                        .sections(row_id)?
                        .find(|index_section| index_section.section == Target::gimli_id())
                        .unwrap();
                    let adjusted_offset: u64 = contribution.offset.0 + adjustment;
                    adjustment += str_offset_section.size as u64;

                    Ok(Some(Contribution {
                        offset: ContributionOffset(adjusted_offset),
                        size: str_offset_section.size as u64,
                    }))
                }
                // dwp input without section
                (Some(_), None) => Ok(None),
                // dwo input with section
                (None, Some(contribution)) => Ok(Some(contribution)),
                // dwo input without section
                (None, None) => Ok(None),
            }
        },
    ))
}

/// In-progress DWARF package being produced.
struct OutputPackage<'file, Endian: gimli::Endianity> {
    /// Object file being created.
    obj: write::Object<'file>,

    /// Format of the DWARF package being created.
    format: PackageFormat,
    /// Endianness of the DWARF package being created.
    endian: Endian,

    /// Identifier for the `.debug_cu_index.dwo` section in the object file being created. Format
    /// depends on whether this is a GNU extension-flavoured package or DWARF 5-flavoured package.
    debug_cu_index: LazySectionId<DebugCuIndex>,
    /// Identifier for the `.debug_tu_index.dwo` section in the object file being created. Format
    /// depends on whether this is a GNU extension-flavoured package or DWARF 5-flavoured package.
    debug_tu_index: LazySectionId<DebugTuIndex>,

    /// Identifier for the `.debug_info.dwo` section in the object file being created.
    ///
    /// Contains concatenated compilation units from `.debug_info.dwo` sections of input DWARF
    /// objects with matching `DW_AT_GNU_dwo_id` attributes.
    debug_info: LazySectionId<DebugInfo>,
    /// Identifier for the `.debug_abbrev.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_abbrev.dwo` sections from input DWARF objects.
    debug_abbrev: LazySectionId<DebugAbbrev>,
    /// Identifier for the `.debug_str.dwo` section in the object file being created.
    ///
    /// Contains a string table merged from the `.debug_str.dwo` sections of input DWARF
    /// objects.
    debug_str: LazySectionId<DebugStr>,
    /// Identifier for the `.debug_types.dwo` section in the object file being created.
    ///
    /// Contains concatenated type units from `.debug_types.dwo` sections of input DWARF
    /// objects with matching type signatures.
    debug_types: LazySectionId<DebugTypes>,
    /// Identifier for the `.debug_line.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_line.dwo` sections from input DWARF objects.
    debug_line: LazySectionId<DebugLine>,
    /// Identifier for the `.debug_loc.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_loc.dwo` sections from input DWARF objects. Only with DWARF
    /// 4 GNU extension.
    debug_loc: LazySectionId<DebugLoc>,
    /// Identifier for the `.debug_loclists.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_loclists.dwo` sections from input DWARF objects. Only with
    /// DWARF 5.
    debug_loclists: LazySectionId<DebugLocLists>,
    /// Identifier for the `.debug_rnglists.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_rnglists.dwo` sections from input DWARF objects. Only with
    /// DWARF 5.
    debug_rnglists: LazySectionId<DebugRngLists>,
    /// Identifier for the `.debug_str_offsets.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_str_offsets.dwo` sections from input DWARF objects,
    /// re-written with offsets into the merged `.debug_str.dwo` section.
    debug_str_offsets: LazySectionId<DebugStrOffsets>,
    /// Identifier for the `.debug_macinfo.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_macinfo.dwo` sections from input DWARF objects. Only with
    /// DWARF 4 GNU extension.
    debug_macinfo: LazySectionId<DebugMacinfo>,
    /// Identifier for the `.debug_macro.dwo` section in the object file being created.
    ///
    /// Contains concatenated `.debug_macro.dwo` sections from input DWARF objects.
    debug_macro: LazySectionId<DebugMacro>,

    /// Compilation unit index entries (offsets + sizes) being accumulated.
    cu_index_entries: Vec<CuIndexEntry>,
    /// Type unit index entries (offsets + sizes) being accumulated.
    tu_index_entries: Vec<TuIndexEntry>,

    /// In-progress string table being accumulated. Used to write final `.debug_str.dwo` and
    /// `.debug_str_offsets.dwo` for each DWARF object.
    string_table: DwpStringTable<Endian>,

    /// `DebugTypeSignature`s of type units and `DwoId`s of compilation units that have already
    /// been added to the output package.
    ///
    /// Used when adding new TU index entries to de-duplicate type units (as required by the
    /// specification). Also used to check that all dwarf objects referenced by executables
    /// have been found.
    seen_units: HashSet<DwarfObjectIdentifier>,
}

impl<'file, Endian: gimli::Endianity> OutputPackage<'file, Endian> {
    /// Return the `SectionId` corresponding to a `gimli::SectionId`, creating a id if it hasn't
    /// been created before.
    ///
    /// Don't call this function if the returned id isn't going to be used, otherwise an empty
    /// section would be created.
    fn section(&mut self, id: gimli::SectionId) -> SectionId {
        use gimli::SectionId::*;
        match id {
            DebugCuIndex => self.debug_cu_index.get(&mut self.obj),
            DebugTuIndex => self.debug_tu_index.get(&mut self.obj),
            DebugInfo => self.debug_info.get(&mut self.obj),
            DebugAbbrev => self.debug_abbrev.get(&mut self.obj),
            DebugStr => self.debug_str.get(&mut self.obj),
            DebugTypes => self.debug_types.get(&mut self.obj),
            DebugLine => self.debug_line.get(&mut self.obj),
            DebugLoc => self.debug_loc.get(&mut self.obj),
            DebugLocLists => self.debug_loclists.get(&mut self.obj),
            DebugRngLists => self.debug_rnglists.get(&mut self.obj),
            DebugStrOffsets => self.debug_str_offsets.get(&mut self.obj),
            DebugMacinfo => self.debug_macinfo.get(&mut self.obj),
            DebugMacro => self.debug_macro.get(&mut self.obj),
            _ => panic!("section invalid in dwarf package"),
        }
    }

    /// Append the contents of a section from the input DWARF object to the equivalent section in
    /// the output object, with no further processing.
    #[tracing::instrument(level = "trace", skip(input))]
    fn append_section<'input, 'output>(
        &mut self,
        input: &object::File<'input>,
        input_id: gimli::SectionId,
        required: bool,
    ) -> Result<Option<Contribution>> {
        let name = dwo_name(input_id);
        match input.section_by_name(name) {
            Some(section) => {
                let size = section.size();
                let data = section.compressed_data()?.decompress()?;
                if !data.is_empty() {
                    let id = self.section(input_id);
                    let offset = self.obj.append_section_data(id, &data, section.align());
                    Ok(Some(Contribution { offset: ContributionOffset(offset), size }))
                } else {
                    Ok(None)
                }
            }
            None if required => Err(anyhow!(DwpError::DwarfObjectMissingSection(name.to_string()))),
            None => Ok(None),
        }
    }

    /// Read the string offsets from `.debug_str_offsets.dwo` in the DWARF object, adding each to
    /// the in-progress `.debug_str` (`DwpStringTable`) and building a new `.debug_str_offsets.dwo`
    /// to be the current DWARF object's contribution to the DWARF package.
    #[tracing::instrument(level = "trace", skip(arena_compression, input, input_dwarf))]
    fn append_str_offsets<'input, 'output, 'arena: 'input>(
        &mut self,
        arena_compression: &'arena Arena<Vec<u8>>,
        input: &object::File<'input>,
        input_dwarf: &gimli::Dwarf<DwpReader<'arena>>,
    ) -> Result<Option<Contribution>> {
        let section_name = gimli::SectionId::DebugStrOffsets.dwo_name().unwrap();
        let section = match input.section_by_name(section_name) {
            Some(section) => section,
            // `.debug_str_offsets.dwo` is an optional section.
            None => return Ok(None),
        };
        let section_size = section.size();

        let mut data = EndianVec::new(self.endian);

        let root_header = input_dwarf.units().next()?.context(DwpError::DwarfObjectWithNoUnits)?;
        let format = PackageFormat::from_dwarf_version(root_header.version());
        let encoding = root_header.encoding();
        // `DebugStrOffsetsBase` knows to skip past the header with DWARF 5.
        let base: gimli::DebugStrOffsetsBase<usize> =
            DebugStrOffsetsBase::default_for_encoding_and_file(encoding, DwarfFileType::Dwo);

        // Copy the DWARF 5 header exactly.
        if format == PackageFormat::DwarfStd {
            // `DebugStrOffsetsBase` should start from after DWARF 5's header, check that.
            assert!(base.0 != 0);
            let header_data = section
                .compressed_data_range(
                    arena_compression,
                    0,
                    base.0.try_into().expect("base offset is larger than a u64"),
                )?
                .ok_or(DwpError::Dwarf5StrOffsetWithoutHeader)?;
            data.write(&header_data)?;
        }

        let entry_size = match encoding.format {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 8,
        };

        let num_elements = section_size / entry_size;
        debug!(?section_size, ?num_elements);

        for i in 0..num_elements {
            let dwo_index = DebugStrOffsetsIndex(i as usize);
            let dwo_offset =
                input_dwarf.debug_str_offsets.get_str_offset(encoding.format, base, dwo_index)?;
            let dwo_str = input_dwarf.debug_str.get_str(dwo_offset)?;
            let dwo_str = dwo_str.to_string()?;

            let dwp_offset = self.string_table.get_or_insert(dwo_str.as_ref())?;
            debug!(?i, ?dwo_str, "dwo_offset={:#x} dwp_offset={:#x}", dwo_offset.0, dwp_offset.0);

            match encoding.format {
                Format::Dwarf32 => {
                    data.write_u32(
                        dwp_offset.0.try_into().context(DwpError::DwpStrOffsetOutOfRange)?,
                    )?;
                }
                Format::Dwarf64 => {
                    data.write_u64(
                        dwp_offset.0.try_into().context(DwpError::DwpStrOffsetOutOfRange)?,
                    )?;
                }
            }
        }

        if num_elements > 0 {
            let id = self.debug_str_offsets.get(&mut self.obj);
            let offset = self.obj.append_section_data(id, data.slice(), section.align());
            Ok(Some(Contribution {
                offset: ContributionOffset(offset),
                size: section_size.try_into().expect("too large for u32"),
            }))
        } else {
            Ok(None)
        }
    }

    /// Append a unit from the input DWARF object to the `.debug_info` (or `.debug_types`) section
    /// in the output object. Only appends unit if it has a `DwarfObjectIdentifier` matching the
    /// target `DwarfObjectIdentifier`.
    #[tracing::instrument(
        level = "trace",
        skip(arena_compression, section, unit, append_cu_contribution, append_tu_contribution)
    )]
    fn append_unit<'input, 'arena: 'input, 'output: 'arena, CuOp, Sect, TuOp>(
        &mut self,
        arena_compression: &'arena Arena<Vec<u8>>,
        section: &Sect,
        unit: &gimli::Unit<DwpReader<'arena>>,
        mut append_cu_contribution: CuOp,
        mut append_tu_contribution: TuOp,
    ) -> Result<()>
    where
        CuOp: FnMut(&mut Self, DwoId, Contribution) -> Result<()>,
        TuOp: FnMut(&mut Self, DebugTypeSignature, Contribution) -> Result<()>,
        Sect: ObjectSection<'input>,
    {
        let size: u64 = unit.header.length_including_self().try_into().unwrap();
        let offset = unit.header.offset();

        let identifier = dwo_identifier_of_unit(&unit);
        match (unit.header.type_(), identifier) {
            (
                UnitType::Compilation | UnitType::SplitCompilation(..),
                Some(DwarfObjectIdentifier::Compilation(dwo_id)),
            ) => {
                debug!(?dwo_id, "compilation unit");
                if self.seen_units.contains(&DwarfObjectIdentifier::Compilation(dwo_id)) {
                    // Return early if a unit with this type signature has already been seen.
                    warn!("skipping {:?}, already seen", dwo_id);
                    return Ok(());
                }

                let offset = offset.as_debug_info_offset().unwrap().0;
                let data = section
                    .compressed_data_range(arena_compression, offset.try_into().unwrap(), size)?
                    .ok_or(DwpError::CompilationUnitWithNoData)?;

                if !data.is_empty() {
                    let id = self.debug_info.get(&mut self.obj);
                    let offset = self.obj.append_section_data(id, data, section.align());
                    let contribution = Contribution { offset: ContributionOffset(offset), size };
                    append_cu_contribution(self, dwo_id, contribution)?;
                    self.seen_units.insert(DwarfObjectIdentifier::Compilation(dwo_id));
                }

                Ok(())
            }
            (
                UnitType::Type { .. } | UnitType::SplitType { .. },
                Some(DwarfObjectIdentifier::Type(type_signature)),
            ) => {
                debug!(?type_signature, "type unit");
                if self.seen_units.contains(&DwarfObjectIdentifier::Type(type_signature)) {
                    // Return early if a unit with this type signature has already been seen.
                    warn!("skipping {:?}, already seen", type_signature);
                    return Ok(());
                }

                let offset = match self.format {
                    PackageFormat::GnuExtension => offset.as_debug_types_offset().unwrap().0,
                    PackageFormat::DwarfStd => offset.as_debug_info_offset().unwrap().0,
                };
                let data = section
                    .compressed_data_range(arena_compression, offset.try_into().unwrap(), size)?
                    .ok_or(DwpError::CompilationUnitWithNoData)?;

                if !data.is_empty() {
                    let id = match self.format {
                        PackageFormat::GnuExtension => self.debug_types.get(&mut self.obj),
                        PackageFormat::DwarfStd => self.debug_info.get(&mut self.obj),
                    };
                    let offset = self.obj.append_section_data(id, data, section.align());
                    let contribution = Contribution { offset: ContributionOffset(offset), size };
                    append_tu_contribution(self, type_signature, contribution)?;
                    self.seen_units.insert(DwarfObjectIdentifier::Type(type_signature));
                }

                Ok(())
            }
            (_, Some(..)) => {
                Err(anyhow!(DwpError::DwarfObjectCompilationUnitWithDwoIdNotSplitUnit))
            }
            (_, None) => Ok(()),
        }
    }

    /// Process a DWARF object. Copies relevant sections, compilation/type units and strings from
    /// DWARF object into output object.
    #[tracing::instrument(level = "trace", skip(arena_compression, input, input_dwarf))]
    fn append_dwarf_object<'input, 'output, 'arena: 'input>(
        &mut self,
        arena_compression: &'arena Arena<Vec<u8>>,
        input: &object::File<'input>,
        input_dwarf: &gimli::Dwarf<DwpReader<'arena>>,
        path: PathBuf,
    ) -> Result<()> {
        use gimli::SectionId::*;

        // Concatenate contents of sections from the DWARF object into the corresponding section in
        // the output.
        let debug_abbrev = self
            .append_section(&input, DebugAbbrev, true)?
            .expect("required section didn't return error");
        let debug_line = self.append_section(&input, DebugLine, false)?;
        let debug_macro = self.append_section(&input, DebugMacro, false)?;

        let (debug_loc, debug_macinfo, debug_loclists, debug_rnglists) = match self.format {
            PackageFormat::GnuExtension => {
                // Only `.debug_loc.dwo` and `.debug_macinfo.dwo` with the GNU extension.
                let debug_loc = self.append_section(&input, DebugLoc, false)?;
                let debug_macinfo = self.append_section(&input, DebugMacinfo, false)?;
                (debug_loc, debug_macinfo, None, None)
            }
            PackageFormat::DwarfStd => {
                // Only `.debug_loclists.dwo` and `.debug_rnglists.dwo` with DWARF 5.
                let debug_loclists = self.append_section(&input, DebugLocLists, false)?;
                let debug_rnglists = self.append_section(&input, DebugRngLists, false)?;
                (None, None, debug_loclists, debug_rnglists)
            }
        };

        // Concatenate string offsets from the DWARF object into the `.debug_str_offsets` section
        // in the output, rewriting offsets to be based on the new, merged string table.
        let debug_str_offsets = self.append_str_offsets(arena_compression, &input, &input_dwarf)?;

        // Load index sections (if they exist).
        let cu_index = maybe_load_index_section::<_, gimli::DebugCuIndex<_>, _>(
            arena_compression,
            self.endian,
            input,
        )?;
        let tu_index = maybe_load_index_section::<_, gimli::DebugTuIndex<_>, _>(
            arena_compression,
            self.endian,
            input,
        )?;

        // Create offset adjustor functions, see comment on `create_contribution_adjustor` for
        // explanation.
        let mut abbrev_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugAbbrev, _>(cu_index.as_ref())?;
        let mut line_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugLine, _>(cu_index.as_ref())?;
        let mut loc_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugLoc, _>(cu_index.as_ref())?;
        let mut loclists_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugLocLists, _>(cu_index.as_ref())?;
        let mut rnglists_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugRngLists, _>(cu_index.as_ref())?;
        let mut str_offsets_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugStrOffsets, _>(cu_index.as_ref())?;
        let mut macinfo_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugMacinfo, _>(cu_index.as_ref())?;
        let mut macro_cu_adjustor =
            create_contribution_adjustor::<_, crate::DebugMacro, _>(cu_index.as_ref())?;

        let mut abbrev_tu_adjustor =
            create_contribution_adjustor::<_, crate::DebugAbbrev, _>(tu_index.as_ref())?;
        let mut line_tu_adjustor =
            create_contribution_adjustor::<_, crate::DebugLine, _>(tu_index.as_ref())?;
        let mut str_offsets_tu_adjustor =
            create_contribution_adjustor::<_, crate::DebugStrOffsets, _>(tu_index.as_ref())?;

        let debug_info_name = gimli::SectionId::DebugInfo.dwo_name().unwrap();
        let debug_info_section = input
            .section_by_name(debug_info_name)
            .with_context(|| DwpError::DwarfObjectMissingSection(debug_info_name.to_string()))?;

        // Append compilation (and type units, in DWARF 5) from `.debug_info`.
        let mut iter = input_dwarf.units();
        while let Some(header) = iter.next()? {
            let unit = input_dwarf.unit(header)?;
            self.append_unit(
                arena_compression,
                &debug_info_section,
                &unit,
                |this, dwo_id, debug_info| {
                    let debug_abbrev = abbrev_cu_adjustor(dwo_id, Some(debug_abbrev))?.unwrap();
                    let debug_line = line_cu_adjustor(dwo_id, debug_line)?;
                    let debug_loc = loc_cu_adjustor(dwo_id, debug_loc)?;
                    let debug_loclists = loclists_cu_adjustor(dwo_id, debug_loclists)?;
                    let debug_rnglists = rnglists_cu_adjustor(dwo_id, debug_rnglists)?;
                    let debug_str_offsets = str_offsets_cu_adjustor(dwo_id, debug_str_offsets)?;
                    let debug_macinfo = macinfo_cu_adjustor(dwo_id, debug_macinfo)?;
                    let debug_macro = macro_cu_adjustor(dwo_id, debug_macro)?;

                    this.cu_index_entries.push(CuIndexEntry {
                        dwo_id,
                        debug_info,
                        debug_abbrev,
                        debug_line,
                        debug_loc,
                        debug_loclists,
                        debug_rnglists,
                        debug_str_offsets,
                        debug_macinfo,
                        debug_macro,
                    });
                    Ok(())
                },
                |this, type_sig, debug_info| {
                    let debug_abbrev = abbrev_tu_adjustor(type_sig, Some(debug_abbrev))?.unwrap();
                    let debug_line = line_tu_adjustor(type_sig, debug_line)?;
                    let debug_str_offsets = str_offsets_tu_adjustor(type_sig, debug_str_offsets)?;

                    this.tu_index_entries.push(TuIndexEntry {
                        type_signature: type_sig,
                        debug_info_or_types: debug_info,
                        debug_abbrev,
                        debug_line,
                        debug_str_offsets,
                    });
                    Ok(())
                },
            )?;
        }

        // Append type units from `.debug_info` with the GNU extension.
        if self.format == PackageFormat::GnuExtension {
            let debug_types_name = gimli::SectionId::DebugTypes.dwo_name().unwrap();
            if let Some(debug_types_section) = input.section_by_name(debug_types_name) {
                let mut iter = input_dwarf.type_units();
                while let Some(header) = iter.next()? {
                    let unit = input_dwarf.unit(header)?;
                    self.append_unit(
                        arena_compression,
                        &debug_types_section,
                        &unit,
                        |_, _, _| {
                            /* no-op, no compilation units in `.debug_types` */
                            Ok(())
                        },
                        |this, type_sig, debug_info_or_types| {
                            let debug_abbrev =
                                abbrev_tu_adjustor(type_sig, Some(debug_abbrev))?.unwrap();
                            let debug_line = line_tu_adjustor(type_sig, debug_line)?;
                            let debug_str_offsets =
                                str_offsets_tu_adjustor(type_sig, debug_str_offsets)?;

                            this.tu_index_entries.push(TuIndexEntry {
                                type_signature: type_sig,
                                debug_info_or_types,
                                debug_abbrev,
                                debug_line,
                                debug_str_offsets,
                            });
                            Ok(())
                        },
                    )?;
                }
            }
        }

        Ok(())
    }

    fn emit(mut self, buffer: &mut dyn object::write::WritableBuffer) -> Result<()> {
        // Write `.debug_str` to the object.
        let _ = self.string_table.write(&mut self.debug_str, &mut self.obj);

        // Write `.debug_{cu,tu}_index` sections to the object.
        debug!("writing cu index");
        self.cu_index_entries.write_index(
            self.endian,
            self.format,
            &mut self.obj,
            &mut self.debug_cu_index,
        )?;
        debug!("writing tu index");
        self.tu_index_entries.write_index(
            self.endian,
            self.format,
            &mut self.obj,
            &mut self.debug_tu_index,
        )?;

        // Write the contents of the entire object to the buffer.
        self.obj.emit(buffer).map_err(From::from)
    }
}

impl<'file, Endian: gimli::Endianity> fmt::Debug for OutputPackage<'file, Endian> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OutputPackage({})", self.format)
    }
}

/// New-type'd index (constructed from `gimli::DwoID`) with a custom `Debug` implementation to
/// print in hexadecimal.
#[derive(Copy, Clone, Eq, Hash, PartialEq)]
struct DwoId(u64);

impl Bucketable for DwoId {
    fn index(&self) -> u64 {
        self.0
    }
}

impl fmt::Debug for DwoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DwoId({:#x})", self.0)
    }
}

impl From<gimli::DwoId> for DwoId {
    fn from(dwo_id: gimli::DwoId) -> Self {
        Self(dwo_id.0)
    }
}

/// New-type'd index (constructed from `gimli::DebugTypeSignature`) with a custom `Debug`
/// implementation to print in hexadecimal.
#[derive(Copy, Clone, Eq, Hash, PartialEq)]
struct DebugTypeSignature(u64);

impl Bucketable for DebugTypeSignature {
    fn index(&self) -> u64 {
        self.0
    }
}

impl fmt::Debug for DebugTypeSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DebugTypeSignature({:#x})", self.0)
    }
}

impl From<gimli::DebugTypeSignature> for DebugTypeSignature {
    fn from(signature: gimli::DebugTypeSignature) -> Self {
        Self(signature.0)
    }
}

/// Identifier for a DWARF object.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
enum DwarfObjectIdentifier {
    /// `DwoId` identifying compilation units.
    Compilation(DwoId),
    /// `DebugTypeSignature` identifying type units.
    Type(DebugTypeSignature),
}

/// New-type'd index from `IndexVec` of strings inserted into the `.debug_str` section.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct DwpStringId(usize);

/// DWARF packages need to merge the `.debug_str` sections of input DWARF objects.
/// `.debug_str_offsets` sections then need to be rebuilt with offsets into the new merged
/// `.debug_str` section and then concatenated (indices into each dwarf object's offset list will
/// therefore still refer to the same string).
///
/// Gimli's `StringTable` produces a `.debug_str` section with a single `.debug_str_offsets`
/// section, but `DwpStringTable` accumulates a single `.debug_str` section and can be used to
/// produce multiple `.debug_str_offsets` sections (which will be concatenated) which all offset
/// into the same `.debug_str`.
struct DwpStringTable<E: gimli::Endianity> {
    debug_str: gimli::write::DebugStr<EndianVec<E>>,
    strings: IndexSet<Vec<u8>>,
    offsets: HashMap<DwpStringId, DebugStrOffset>,
}

impl<E: gimli::Endianity> DwpStringTable<E> {
    /// Create a new `DwpStringTable` with a given endianity.
    fn new(endianness: E) -> Self {
        Self {
            debug_str: gimli::write::DebugStr(EndianVec::new(endianness)),
            strings: IndexSet::new(),
            offsets: HashMap::new(),
        }
    }

    /// Insert a string into the string table and return its offset in the table. If the string is
    /// already in the table, returns its offset.
    fn get_or_insert<T: Into<Vec<u8>>>(&mut self, bytes: T) -> Result<DebugStrOffset> {
        let bytes = bytes.into();
        assert!(!bytes.contains(&0));
        let (index, is_new) = self.strings.insert_full(bytes.clone());
        let index = DwpStringId(index);
        if !is_new {
            return Ok(*self.offsets.get(&index).expect("insert exists but no offset"));
        }

        // Keep track of the offset for this string, it might be referenced by the next compilation
        // unit too.
        let offset = self.debug_str.offset();
        self.offsets.insert(index, offset);

        // Insert into the string table.
        self.debug_str.write(&bytes)?;
        self.debug_str.write_u8(0)?;

        Ok(offset)
    }

    /// Write the accumulated `.debug_str` section to an object file, returns the offset of the
    /// section in the object (if there was a `.debug_str` section to write at all).
    fn write<'output>(
        self,
        debug_str: &mut LazySectionId<DebugStr>,
        obj: &mut write::Object<'output>,
    ) -> Option<u64> {
        let data = self.debug_str.0.slice();
        if !data.is_empty() {
            // FIXME: what is the correct way to determine this alignment
            let id = debug_str.get(obj);
            Some(obj.append_section_data(id, data, 1))
        } else {
            None
        }
    }
}

/// Helper trait for types that can be used in creating the `.debug_{cu,tu}_index` hash table.
trait Bucketable {
    fn index(&self) -> u64;
}

/// New-type'd offset into a section of a compilation/type unit's contribution.
#[derive(Copy, Clone, Eq, Hash, PartialEq)]
struct ContributionOffset(u64);

impl fmt::Debug for ContributionOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContributionOffset({:#x})", self.0)
    }
}

trait IndexEntry: Bucketable {
    /// Return the number of columns in `.debug_{cu,tu}_index` required by this entry.
    fn number_of_columns(&self, format: PackageFormat) -> u32;

    /// Return the signature of the entry (`DwoId` or `DebugTypeSignature`).
    fn signature(&self) -> u64;

    /// Write the header for the index entry (e.g. `gimli::DW_SECT_INFO` constants)
    ///
    /// Only uses the entry to know which columns exist (invariant: every entry has the same
    /// number of columns).
    fn write_header<Endian: gimli::Endianity>(
        &self,
        format: PackageFormat,
        out: &mut EndianVec<Endian>,
    ) -> Result<()>;

    /// Write the contribution for the index entry to `out`, component of `Contribution` written is
    /// chosen by `proj` closure.
    fn write_contribution<Endian, Proj>(
        &self,
        format: PackageFormat,
        out: &mut EndianVec<Endian>,
        proj: Proj,
    ) -> Result<()>
    where
        Endian: gimli::Endianity,
        Proj: Fn(Contribution) -> u32;
}

impl<T: IndexEntry> Bucketable for T {
    fn index(&self) -> u64 {
        self.signature()
    }
}

/// Type alias for the size of a compilation/type unit's contribution.
type ContributionSize = u64;

/// Contribution to a section - offset and size.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct Contribution {
    /// Offset of this contribution into its containing section.
    offset: ContributionOffset,
    /// Size of this contribution in its containing section.
    size: ContributionSize,
}

/// Entry into the `.debug_tu_index` section.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct TuIndexEntry {
    type_signature: DebugTypeSignature,
    debug_info_or_types: Contribution,
    debug_abbrev: Contribution,
    debug_line: Option<Contribution>,
    debug_str_offsets: Option<Contribution>,
}

impl IndexEntry for TuIndexEntry {
    fn number_of_columns(&self, _: PackageFormat) -> u32 {
        2 /* info/types and abbrev are required columns */
        + self.debug_line.map_or(0, |_| 1)
        + self.debug_str_offsets.map_or(0, |_| 1)
    }

    fn signature(&self) -> u64 {
        self.type_signature.0
    }

    fn write_header<Endian: gimli::Endianity>(
        &self,
        format: PackageFormat,
        out: &mut EndianVec<Endian>,
    ) -> Result<()> {
        match format {
            PackageFormat::GnuExtension => {
                out.write_u32(gimli::DW_SECT_V2_TYPES.0)?;
                out.write_u32(gimli::DW_SECT_V2_ABBREV.0)?;
                if self.debug_line.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_LINE.0)?;
                }
                if self.debug_str_offsets.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_STR_OFFSETS.0)?;
                }
            }
            PackageFormat::DwarfStd => {
                out.write_u32(gimli::DW_SECT_INFO.0)?;
                out.write_u32(gimli::DW_SECT_ABBREV.0)?;
                if self.debug_line.is_some() {
                    out.write_u32(gimli::DW_SECT_LINE.0)?;
                }
                if self.debug_str_offsets.is_some() {
                    out.write_u32(gimli::DW_SECT_STR_OFFSETS.0)?;
                }
            }
        }

        Ok(())
    }

    fn write_contribution<Endian, Proj>(
        &self,
        _: PackageFormat,
        out: &mut EndianVec<Endian>,
        proj: Proj,
    ) -> Result<()>
    where
        Endian: gimli::Endianity,
        Proj: Fn(Contribution) -> u32,
    {
        out.write_u32(proj(self.debug_info_or_types))?;
        out.write_u32(proj(self.debug_abbrev))?;
        if let Some(debug_line) = self.debug_line {
            out.write_u32(proj(debug_line))?;
        }
        if let Some(debug_str_offsets) = self.debug_str_offsets {
            out.write_u32(proj(debug_str_offsets))?;
        }
        Ok(())
    }
}

/// Entry into the `.debug_cu_index` section.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct CuIndexEntry {
    dwo_id: DwoId,
    debug_info: Contribution,
    debug_abbrev: Contribution,
    debug_line: Option<Contribution>,
    debug_loc: Option<Contribution>,
    debug_loclists: Option<Contribution>,
    debug_rnglists: Option<Contribution>,
    debug_str_offsets: Option<Contribution>,
    debug_macinfo: Option<Contribution>,
    debug_macro: Option<Contribution>,
}

impl IndexEntry for CuIndexEntry {
    fn number_of_columns(&self, format: PackageFormat) -> u32 {
        match format {
            PackageFormat::GnuExtension => {
                2 /* info and abbrev are required columns */
                + self.debug_line.map_or(0, |_| 1)
                + self.debug_loc.map_or(0, |_| 1)
                + self.debug_str_offsets.map_or(0, |_| 1)
                + self.debug_macinfo.map_or(0, |_| 1)
                + self.debug_macro.map_or(0, |_| 1)
            }
            PackageFormat::DwarfStd => {
                2 /* info and abbrev are required columns */
                + self.debug_line.map_or(0, |_| 1)
                + self.debug_loclists.map_or(0, |_| 1)
                + self.debug_rnglists.map_or(0, |_| 1)
                + self.debug_str_offsets.map_or(0, |_| 1)
                + self.debug_macro.map_or(0, |_| 1)
            }
        }
    }

    fn signature(&self) -> u64 {
        self.dwo_id.0
    }

    fn write_header<Endian: gimli::Endianity>(
        &self,
        format: PackageFormat,
        out: &mut EndianVec<Endian>,
    ) -> Result<()> {
        match format {
            PackageFormat::GnuExtension => {
                out.write_u32(gimli::DW_SECT_V2_INFO.0)?;
                out.write_u32(gimli::DW_SECT_V2_ABBREV.0)?;
                if self.debug_line.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_LINE.0)?;
                }
                if self.debug_loc.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_LOC.0)?;
                }
                if self.debug_str_offsets.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_STR_OFFSETS.0)?;
                }
                if self.debug_macinfo.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_MACINFO.0)?;
                }
                if self.debug_macro.is_some() {
                    out.write_u32(gimli::DW_SECT_V2_MACRO.0)?;
                }
            }
            PackageFormat::DwarfStd => {
                out.write_u32(gimli::DW_SECT_INFO.0)?;
                out.write_u32(gimli::DW_SECT_ABBREV.0)?;
                if self.debug_line.is_some() {
                    out.write_u32(gimli::DW_SECT_LINE.0)?;
                }
                if self.debug_loclists.is_some() {
                    out.write_u32(gimli::DW_SECT_LOCLISTS.0)?;
                }
                if self.debug_rnglists.is_some() {
                    out.write_u32(gimli::DW_SECT_RNGLISTS.0)?;
                }
                if self.debug_str_offsets.is_some() {
                    out.write_u32(gimli::DW_SECT_STR_OFFSETS.0)?;
                }
                if self.debug_macro.is_some() {
                    out.write_u32(gimli::DW_SECT_MACRO.0)?;
                }
            }
        }

        Ok(())
    }

    fn write_contribution<Endian, Proj>(
        &self,
        format: PackageFormat,
        out: &mut EndianVec<Endian>,
        proj: Proj,
    ) -> Result<()>
    where
        Endian: gimli::Endianity,
        Proj: Fn(Contribution) -> u32,
    {
        match format {
            PackageFormat::GnuExtension => {
                out.write_u32(proj(self.debug_info))?;
                out.write_u32(proj(self.debug_abbrev))?;
                if let Some(debug_line) = self.debug_line {
                    out.write_u32(proj(debug_line))?;
                }
                if let Some(debug_loc) = self.debug_loc {
                    out.write_u32(proj(debug_loc))?;
                }
                if let Some(debug_str_offsets) = self.debug_str_offsets {
                    out.write_u32(proj(debug_str_offsets))?;
                }
                if let Some(debug_macinfo) = self.debug_macinfo {
                    out.write_u32(proj(debug_macinfo))?;
                }
                if let Some(debug_macro) = self.debug_macro {
                    out.write_u32(proj(debug_macro))?;
                }
            }
            PackageFormat::DwarfStd => {
                out.write_u32(proj(self.debug_info))?;
                out.write_u32(proj(self.debug_abbrev))?;
                if let Some(debug_line) = self.debug_line {
                    out.write_u32(proj(debug_line))?;
                }
                if let Some(debug_loclists) = self.debug_loclists {
                    out.write_u32(proj(debug_loclists))?;
                }
                if let Some(debug_rnglists) = self.debug_rnglists {
                    out.write_u32(proj(debug_rnglists))?;
                }
                if let Some(debug_str_offsets) = self.debug_str_offsets {
                    out.write_u32(proj(debug_str_offsets))?;
                }
                if let Some(debug_macro) = self.debug_macro {
                    out.write_u32(proj(debug_macro))?;
                }
            }
        }

        Ok(())
    }
}

trait IndexCollection<Entry: IndexEntry> {
    /// Write `.debug_{cu,tu}_index` to the output object.
    fn write_index<'output, Endian, Id>(
        &self,
        endianness: Endian,
        format: PackageFormat,
        output: &mut write::Object<'output>,
        output_id: &mut LazySectionId<Id>,
    ) -> Result<()>
    where
        Endian: gimli::Endianity,
        Id: HasGimliId;
}

impl<Entry: IndexEntry + fmt::Debug> IndexCollection<Entry> for Vec<Entry> {
    #[tracing::instrument(level = "trace", skip(output, output_id))]
    fn write_index<'output, Endian, Id>(
        &self,
        endianness: Endian,
        format: PackageFormat,
        output: &mut write::Object<'output>,
        output_id: &mut LazySectionId<Id>,
    ) -> Result<()>
    where
        Endian: gimli::Endianity,
        Id: HasGimliId,
    {
        if self.len() == 0 {
            return Ok(());
        }

        let mut out = EndianVec::new(endianness);

        let buckets = bucket(self);
        debug!(?buckets);

        let num_columns = self[0].number_of_columns(format);
        assert!(self.iter().all(|e| e.number_of_columns(format) == num_columns));
        debug!(?num_columns);

        // Write header..
        match format {
            PackageFormat::GnuExtension => {
                // GNU Extension
                out.write_u32(2)?;
            }
            PackageFormat::DwarfStd => {
                // DWARF 5
                out.write_u16(5)?;
                // Reserved padding
                out.write_u16(0)?;
            }
        }

        // Columns (e.g. info, abbrev, loc, etc.)
        out.write_u32(num_columns)?;
        // Number of units
        out.write_u32(self.len().try_into().unwrap())?;
        // Number of buckets
        out.write_u32(buckets.len().try_into().unwrap())?;

        // Write signatures..
        for i in &buckets {
            if *i > 0 {
                out.write_u64(self[(*i - 1) as usize].signature())?;
            } else {
                out.write_u64(0)?;
            }
        }

        // Write indices..
        for i in &buckets {
            out.write_u32(*i)?;
        }

        // Write column headers..
        self[0].write_header(format, &mut out)?;

        // Write offsets..
        let write_offset = |contrib: Contribution| contrib.offset.0.try_into().unwrap();
        for entry in self {
            entry.write_contribution(format, &mut out, write_offset)?;
        }

        // Write sizes..
        let write_size = |contrib: Contribution| contrib.size.try_into().unwrap();
        for entry in self {
            entry.write_contribution(format, &mut out, write_size)?;
        }

        // FIXME: use the correct alignment here
        let output_id = output_id.get(output);
        let _ = output.append_section_data(output_id, out.slice(), 1);
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "rust-dwp", about = "merge split dwarf (.dwo) files")]
struct Opt {
    /// Specify path to input dwarf objects and packages
    #[structopt(parse(from_os_str))]
    inputs: Vec<PathBuf>,
    /// Specify the executable/library files to get the list of *.dwo from
    #[structopt(short = "e", long = "exec", parse(from_os_str))]
    executables: Option<Vec<PathBuf>>,
    /// Specify the path to write the packaged dwp file to
    #[structopt(short = "o", long = "output", parse(from_os_str), default_value = "-")]
    output: PathBuf,
}

/// Wrapper around output writer which handles differences between stdout, file and pipe outputs.
enum Output {
    Stdout(Stdout),
    File(File),
    Pipe(File),
}

impl Output {
    /// Create a `Output` from the input path (or "-" for stdout).
    fn new<S: AsRef<OsStr>>(path: S) -> Result<Self> {
        let path = path.as_ref();
        if path == "-" {
            return Ok(Output::Stdout(io::stdout()));
        }

        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        if file.metadata()?.file_type().is_fifo() {
            Ok(Output::File(file))
        } else {
            Ok(Output::Pipe(file))
        }
    }
}

impl Write for Output {
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Output::Stdout(stdout) => stdout.flush(),
            Output::Pipe(pipe) => pipe.flush(),
            Output::File(file) => file.flush(),
        }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Output::Stdout(stdout) => stdout.write(buf),
            Output::Pipe(pipe) => pipe.write(buf),
            Output::File(file) => file.write(buf),
        }
    }
}

/// Helper function to return the name of a section in a dwarf object.
///
/// Unnecessary but works around a bug in Gimli.
fn dwo_name(id: gimli::SectionId) -> &'static str {
    match id {
        // TODO: patch gimli to return this
        gimli::SectionId::DebugMacinfo => ".debug_macinfo.dwo",
        _ => id.dwo_name().unwrap(),
    }
}

/// Load and parse an object file.
#[tracing::instrument(level = "trace", skip(arena_mmap))]
fn load_object_file<'input, 'arena: 'input>(
    arena_mmap: &'arena Arena<Mmap>,
    path: &'input Path,
) -> Result<object::File<'arena>> {
    let file = fs::File::open(&path)
        .with_context(|| format!("failed to open object file at {}", path.display()))?;

    let mmap = (unsafe { Mmap::map(&file) })
        .with_context(|| format!("failed to map file at {}", path.display()))?;

    let mmap_ref = (*arena_mmap.alloc(mmap)).borrow();
    object::File::parse(&**mmap_ref)
        .with_context(|| format!("failed to parse file at {}", path.display()))
}

/// Returns the gimli `RunTimeEndian` corresponding to a object `Endianness`.
fn runtime_endian_from_endianness<'a>(endianness: Endianness) -> RunTimeEndian {
    match endianness {
        Endianness::Little => RunTimeEndian::Little,
        Endianness::Big => RunTimeEndian::Big,
    }
}

/// Helper trait to add `compressed_data_range` function to `ObjectSection` types.
trait CompressedDataRangeExt<'input, 'arena: 'input>: ObjectSection<'arena> {
    /// Return the decompressed contents of the section data in the given range.
    fn compressed_data_range(
        &self,
        arena_compression: &'arena Arena<Vec<u8>>,
        address: u64,
        size: u64,
    ) -> object::Result<Option<&'input [u8]>>;
}

impl<'input, 'arena: 'input, S> CompressedDataRangeExt<'input, 'arena> for S
where
    S: ObjectSection<'arena>,
{
    fn compressed_data_range(
        &self,
        arena_compression: &'arena Arena<Vec<u8>>,
        address: u64,
        size: u64,
    ) -> object::Result<Option<&'input [u8]>> {
        let data = self.compressed_data()?.decompress()?;

        /// Originally from `object::read::util`, used in `ObjectSection::data_range`, but not
        /// public.
        fn data_range(
            data: &[u8],
            data_address: u64,
            range_address: u64,
            size: u64,
        ) -> Option<&[u8]> {
            let offset = range_address.checked_sub(data_address)?;
            data.get(offset.try_into().ok()?..)?.get(..size.try_into().ok()?)
        }

        let data_ref = match data {
            Cow::Borrowed(data) => data,
            Cow::Owned(data) => (*arena_compression.alloc(data)).borrow(),
        };
        Ok(data_range(data_ref, self.address(), address, size))
    }
}

/// Loads a section of a file from `object::File` into a `gimli::EndianSlice`. Expected to be
/// curried using a closure and provided to `Dwarf::load`.
#[tracing::instrument(level = "trace", skip(obj, arena_data, arena_relocations))]
fn load_file_section<'input, 'arena: 'input>(
    id: gimli::SectionId,
    obj: &object::File<'input>,
    is_dwo: bool,
    arena_data: &'arena Arena<Cow<'input, [u8]>>,
    arena_relocations: &'arena Arena<RelocationMap>,
) -> Result<DwpReader<'arena>> {
    let mut relocations = RelocationMap::default();
    let name = if is_dwo { id.dwo_name() } else { Some(id.name()) };

    let data = match name.and_then(|name| obj.section_by_name(&name)) {
        Some(ref section) => {
            if !is_dwo {
                add_relocations(&mut relocations, obj, section)?;
            }
            section.compressed_data()?.decompress()?
        }
        // Use a non-zero capacity so that `ReaderOffsetId`s are unique.
        None => Cow::Owned(Vec::with_capacity(1)),
    };

    let data_ref = (*arena_data.alloc(data)).borrow();
    let reader =
        gimli::EndianSlice::new(data_ref, runtime_endian_from_endianness(obj.endianness()));
    let section = reader;
    let relocations = (*arena_relocations.alloc(relocations)).borrow();
    Ok(Relocate { relocations, section, reader })
}

/// Returns the `DwoId` or `DebugTypeSignature` of a unit.
///
/// **DWARF 5:**
///
/// - `DwoId` is in the unit header of a skeleton unit (identifying the split compilation unit
/// that contains the debuginfo) or split compilation unit (identifying the skeleton unit that this
/// debuginfo corresponds to).
/// - `DebugTypeSignature` is in the unit header of a split type unit.
///
/// **Earlier DWARF versions with GNU extension:**
///
/// - `DW_AT_GNU_dwo_id` attribute of the DIE contains the `DwoId`.
#[tracing::instrument(level = "trace", skip(unit))]
fn dwo_identifier_of_unit<R: gimli::Reader>(
    unit: &gimli::Unit<R>,
) -> Option<DwarfObjectIdentifier> {
    match unit.header.type_() {
        // Compilation units with DWARF 5
        UnitType::Skeleton(dwo_id) | UnitType::SplitCompilation(dwo_id) => {
            Some(DwarfObjectIdentifier::Compilation(dwo_id.into()))
        }
        // Compilation units with GNU Extension
        UnitType::Compilation => {
            unit.dwo_id.map(|id| DwarfObjectIdentifier::Compilation(id.into()))
        }
        // Type units with DWARF 5
        UnitType::SplitType { type_signature, .. } => {
            Some(DwarfObjectIdentifier::Type(type_signature.into()))
        }
        // Type units with GNU extension
        UnitType::Type { type_signature, .. } => {
            Some(DwarfObjectIdentifier::Type(type_signature.into()))
        }
        // Wrong compilation unit type.
        _ => None,
    }
}

/// Returns the `TargetDwarfObject` of a compilation/type unit.
///
/// In DWARF 5, skeleton compilation unit will contain a `DW_AT_dwo_name` attribute with the name
/// of the dwarf object file containing the split compilation unit with the `DwoId`. In earlier
/// DWARF versions with GNU extension, `DW_AT_GNU_dwo_name` attribute contains a name.
#[tracing::instrument(level = "trace", skip(dwarf, unit))]
fn dwo_id_and_path_of_unit<R: gimli::Reader>(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
) -> Result<Option<(DwarfObjectIdentifier, PathBuf)>> {
    let identifier = if let Some(identifier) = dwo_identifier_of_unit(unit) {
        identifier
    } else {
        return Ok(None);
    };

    let dwo_name = {
        let mut cursor = unit.header.entries(&unit.abbreviations);
        cursor.next_dfs()?;
        let root = cursor.current().ok_or(anyhow!(DwpError::MissingUnitDie))?;

        let dwo_name = if let Some(val) = root.attr_value(gimli::DW_AT_dwo_name)? {
            // DWARF 5
            val
        } else if let Some(val) = root.attr_value(gimli::DW_AT_GNU_dwo_name)? {
            // GNU Extension
            val
        } else {
            return Err(anyhow!(DwpError::DwoIdWithoutDwoName));
        };

        dwarf.attr_string(&unit, dwo_name)?.to_string()?.into_owned()
    };

    // Prepend the compilation directory if it exists.
    let mut path = if let Some(comp_dir) = &unit.comp_dir {
        PathBuf::from(comp_dir.to_string()?.into_owned())
    } else {
        PathBuf::new()
    };
    path.push(dwo_name);

    Ok(Some((identifier, path)))
}

/// Parse the executable and return the `.debug_addr` section and the referenced DWARF objects.
///
/// Loading DWARF objects requires the `.debug_addr` section from the parent object. DWARF objects
/// that need to be loaded are accumulated from the skeleton compilation units in the executable's
/// DWARF, their `DwoId` and constructed paths are collected.
#[tracing::instrument(level = "trace", skip(obj, arena_data, arena_relocations))]
fn parse_executable<'input, 'arena: 'input>(
    arena_data: &'arena Arena<Cow<'input, [u8]>>,
    arena_relocations: &'arena Arena<RelocationMap>,
    obj: &object::File<'input>,
    target_dwarf_objects: &mut HashSet<DwarfObjectIdentifier>,
    dwarf_object_paths: &mut Vec<PathBuf>,
) -> Result<(PackageFormat, object::Architecture, object::Endianness)> {
    let mut load_section = |id: gimli::SectionId| -> Result<_> {
        load_file_section(id, &obj, false, &arena_data, &arena_relocations)
    };

    let dwarf = gimli::Dwarf::load(&mut load_section)?;

    let format = {
        let root_header = dwarf.units().next()?.context(DwpError::DwarfObjectWithNoUnits)?;
        PackageFormat::from_dwarf_version(root_header.version())
    };

    let mut iter = dwarf.units();
    while let Some(header) = iter.next()? {
        let unit = dwarf.unit(header)?;
        if let Some((target, path)) = dwo_id_and_path_of_unit(&dwarf, &unit)? {
            // Only add `DwoId`s to the target vector, not `DebugTypeSignature`s. There doesn't
            // appear to be a "skeleton type unit" to find the corresponding unit of (there are
            // normal type units in an executable, but should we expect to find a corresponding
            // split type unit for those?).
            if matches!(target, DwarfObjectIdentifier::Compilation(_)) {
                target_dwarf_objects.insert(target);
            }

            dwarf_object_paths.push(path);
        }
    }

    Ok((format, obj.architecture(), obj.endianness()))
}

/// Create an object file with empty sections that will be later populated from DWARF object files.
#[tracing::instrument(level = "trace")]
fn create_output_object<'input, 'output>(
    format: PackageFormat,
    architecture: object::Architecture,
    endianness: object::Endianness,
) -> Result<OutputPackage<'output, RunTimeEndian>> {
    let obj = write::Object::new(BinaryFormat::Elf, architecture, endianness);

    let endian = runtime_endian_from_endianness(endianness);
    let string_table = DwpStringTable::new(endian);

    Ok(OutputPackage {
        obj,
        format,
        endian,
        string_table,
        debug_cu_index: Default::default(),
        debug_tu_index: Default::default(),
        debug_info: Default::default(),
        debug_abbrev: Default::default(),
        debug_str: Default::default(),
        debug_types: Default::default(),
        debug_line: Default::default(),
        debug_loc: Default::default(),
        debug_loclists: Default::default(),
        debug_rnglists: Default::default(),
        debug_str_offsets: Default::default(),
        debug_macinfo: Default::default(),
        debug_macro: Default::default(),
        cu_index_entries: Default::default(),
        tu_index_entries: Default::default(),
        seen_units: Default::default(),
    })
}

/// Returns a hash table computed for `elements`. Used in the `.debug_{cu,tu}_index` sections.
#[tracing::instrument(level = "trace", skip_all)]
fn bucket<B: Bucketable + fmt::Debug>(elements: &[B]) -> Vec<u32> {
    let unit_count: u32 = elements.len().try_into().expect("unit count too big for u32");
    let num_buckets = if elements.len() < 2 { 2 } else { (3 * unit_count / 2).next_power_of_two() };
    let mask: u64 = num_buckets as u64 - 1;
    trace!(?mask);

    let mut buckets = vec![0u32; num_buckets as usize];
    trace!(?buckets);
    for (elem, i) in elements.iter().zip(0u32..) {
        trace!(?i, ?elem);
        let s = elem.index();
        let mut h = s & mask;
        let hp = ((s >> 32) & mask) | 1;
        trace!(?s, ?h, ?hp);

        while buckets[h as usize] > 0 {
            assert!(elements[(buckets[h as usize] - 1) as usize].index() != elem.index());
            h = (h + hp) & mask;
            trace!(?h);
        }

        buckets[h as usize] = i + 1;
        trace!(?buckets);
    }

    buckets
}

fn main() -> Result<()> {
    let subscriber = Registry::default().with(EnvFilter::from_env("RUST_DWP_LOG")).with(
        HierarchicalLayer::default()
            .with_writer(io::stderr)
            .with_indent_lines(true)
            .with_targets(true)
            .with_indent_amount(2),
    );
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let opt = Opt::from_args();
    trace!(?opt);

    let arena_compression = Arena::new();
    let arena_data = Arena::new();
    let arena_mmap = Arena::new();
    let arena_relocations = Arena::new();

    let mut output_object_inputs = None;

    // Paths to DWARF objects to open (from positional arguments or referenced by executables).
    let mut dwarf_object_paths = opt.inputs;
    // `DwoId`s or `DebugTypeSignature`s referenced by any executables that have been opened,
    // must find.
    let mut target_dwarf_objects = HashSet::new();

    if let Some(executables) = opt.executables {
        for executable in &executables {
            let obj = load_object_file(&arena_mmap, executable)?;
            let found_output_object_inputs = parse_executable(
                &arena_data,
                &arena_relocations,
                &obj,
                &mut target_dwarf_objects,
                &mut dwarf_object_paths,
            )?;

            output_object_inputs = output_object_inputs.or(Some(found_output_object_inputs));
        }
    }

    // Need to know the package format, architecture and endianness to create the output object.
    // Retrieve these from the input files - either the executable or the dwarf objects - so delay
    // creation until these inputs are definitely available.
    let mut output = None;

    for path in dwarf_object_paths {
        let dwo_obj = match load_object_file(&arena_mmap, &path) {
            Ok(dwo_obj) => dwo_obj,
            Err(e) => {
                warn!(
                    "could not open object file, dwp may fail later if required unit is not found"
                );
                trace!(?e);
                return Ok(());
            }
        };

        let mut load_dwo_section = |id: gimli::SectionId| -> Result<_> {
            load_file_section(id, &dwo_obj, true, &arena_data, &arena_relocations)
        };

        let dwo_dwarf = gimli::Dwarf::load(&mut load_dwo_section)?;
        let root_header = dwo_dwarf.units().next()?.context(DwpError::DwarfObjectWithNoUnits)?;
        let format = PackageFormat::from_dwarf_version(root_header.version());

        if output.is_none() {
            let (format, architecture, endianness) = match output_object_inputs {
                Some(inpts) => inpts,
                None => (format, dwo_obj.architecture(), dwo_obj.endianness()),
            };
            output = Some(create_output_object(format, architecture, endianness)?);
        }

        if let Some(output) = &mut output {
            output.append_dwarf_object(&arena_compression, &dwo_obj, &dwo_dwarf, path)?;
        }
    }

    if let Some(output) = output {
        if target_dwarf_objects.difference(&output.seen_units).count() != 0 {
            let missing = target_dwarf_objects.difference(&output.seen_units).cloned().collect();
            return Err(anyhow!(DwpError::MissingReferencedUnit(missing)));
        }

        let mut output_stream = StreamingBuffer::new(BufWriter::new(
            Output::new(opt.output).context(DwpError::FailedToCreateOutputFile)?,
        ));
        output.emit(&mut output_stream)?;
        output_stream.result()?;
        output_stream.into_inner().flush()?;
    }

    Ok(())
}
