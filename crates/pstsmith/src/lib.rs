#![doc = include_str!("../README.md")]

use byteorder::WriteBytesExt;
use std::{
    cell::RefMut,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fmt::Debug,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    mem,
    path::Path,
    rc::Rc,
    sync::Mutex,
};
use thiserror::Error;
use tracing::{error, instrument, warn};

#[cfg(feature = "convert")]
pub mod convert;
pub mod ltp;
pub mod messaging;
pub mod ndb;

mod block_sig;
mod crc;
mod encode;
#[doc(hidden)]
pub mod measurement;

/// The writer's test build counts allocations (see `measurement::heap`).
#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: measurement::heap::Counting = measurement::heap::Counting;
mod session;

pub use session::{
    debug_allocation_map, debug_block_references, debug_heap_fill_levels, debug_node_streams,
    UnicodePstWriter,
};
use session::{BlockLookup, CommitPlan, Tx, WriterCore};

use ltp::{
    heap::*, prop_context::*, prop_type::PropertyType, read_write::*, table_context::*, tree::*,
};
use messaging::read_write::{NamedPropertyMapReadWrite, SearchReadWrite};
use messaging::{folder::*, message::*, named_prop::*, search::*, store::*};
use ndb::{
    block::*, block_id::*, block_ref::*, byte_index::*, header::*, node_id::*, page::*,
    read_write::*, root::*, *,
};

#[derive(Error, Debug)]
pub enum PstError {
    #[error("Opened read-only")]
    OpenedReadOnly,
    #[error("Cannot write to file: {0}")]
    NoWriteAccess(String),
    #[error("I/O error: {0:?}")]
    Io(#[from] io::Error),
    #[error("I/O error: {0}")]
    BorrowedIo(String),
    #[error("Failed to lock file")]
    LockError,
    #[error("Integer conversion failed")]
    IntegerConversion,
    #[error("Node Database error: {0}")]
    NodeDatabaseError(#[from] NdbError),
    #[error("AllocationMapPage not found: {0}")]
    AllocationMapPageNotFound(usize),
    #[error("Invalid BTree page: offset: 0x{0:X}")]
    InvalidBTreePage(u64),
}

impl From<&PstError> for io::Error {
    fn from(err: &PstError) -> Self {
        match err {
            PstError::NoWriteAccess(path) => {
                Self::new(io::ErrorKind::PermissionDenied, path.as_str())
            }
            err => Self::other(format!("{err:?}")),
        }
    }
}

impl From<PstError> for io::Error {
    fn from(err: PstError) -> Self {
        match err {
            PstError::NoWriteAccess(path) => {
                Self::new(io::ErrorKind::PermissionDenied, path.as_str())
            }
            PstError::Io(err) => err,
            err => Self::other(err),
        }
    }
}

impl From<&io::Error> for PstError {
    fn from(err: &io::Error) -> Self {
        Self::BorrowedIo(format!("{err:?}"))
    }
}

type PstResult<T> = std::result::Result<T, PstError>;

/// The methods on this trait and the [`PstFileInner`] struct are not public, PST modifications
/// have to go through `pub fn` methods on the [`PstFileLockGuard`] type which encapsulates a `dyn`
/// reference to this trait.
trait PstFileLock<Pst>
where
    Pst: PstFile,
{
    fn start_write(&mut self) -> io::Result<()>;
    fn finish_write(&mut self) -> io::Result<()>;

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Pst as PstFile>::BlockBTree>>;
    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Pst as PstFile>::NodeBTree>>;
}

/// This is the public interface for writing to a PST.
pub struct PstFileLockGuard<'a, Pst>
where
    Pst: PstFile,
{
    pst: &'a mut dyn PstFileLock<Pst>,
}

impl<'a, Pst> PstFileLockGuard<'a, Pst>
where
    Pst: PstFile,
{
    fn new(pst: &'a mut dyn PstFileLock<Pst>) -> io::Result<Self> {
        pst.start_write()?;
        Ok(Self { pst })
    }

    /// Explicitly flush pending updates to the PST file. This will still happen implicitly when
    /// the [`PstFileLockGuard`] is dropped, but this allows you to handle errors.
    #[instrument(skip_all)]
    pub fn flush(&mut self) -> io::Result<()> {
        self.pst.finish_write().inspect_err(|err| {
            error!(
                name: "PstFinishWriteFailed",
                ?err,
                "PstFileLock::finish_write failed"
            );
        })?;

        Ok(())
    }
}

impl<Pst> Drop for PstFileLockGuard<'_, Pst>
where
    Pst: PstFile,
{
    #[instrument(skip_all)]
    fn drop(&mut self) {
        if let Err(err) = self.flush() {
            error!(
                name: "PstFileLockGuardFlushFailed",
                ?err,
                "Writing to the PST file failed"
            );
        }
    }
}

pub trait PstReader: Read + Seek {}

impl<T> PstReader for T where T: Read + Seek {}

/// [PST File](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/6b57253b-0853-47bb-99bb-d4b8f78105f0)
pub trait PstFile: Sized {
    type BlockId: BlockId<Index = Self::BTreeKey> + BlockIdReadWrite;
    type PageId: BlockId<Index = Self::BTreeKey> + BlockIdReadWrite;
    type ByteIndex: ByteIndex + ByteIndexReadWrite;
    type BlockRef: BlockRef<Block = Self::BlockId, Index = Self::ByteIndex> + BlockRefReadWrite;
    type PageRef: BlockRef<Block = Self::PageId, Index = Self::ByteIndex> + BlockRefReadWrite;
    type Root: Root<Self>;
    type Header: Header<Self>;
    type PageTrailer: PageTrailer<BlockId = Self::PageId> + PageTrailerReadWrite;
    type BTreeKey: BTreeEntryKey;
    type NodeBTreeEntry: NodeBTreeEntry<Block = Self::BlockId> + BTreeEntry<Key = Self::BTreeKey>;
    type NodeBTree: NodeBTree<Self, Self::NodeBTreeEntry>;
    type BlockBTreeEntry: BlockBTreeEntry<Block = Self::BlockRef> + BTreeEntry<Key = Self::BTreeKey>;
    type BlockBTree: BlockBTree<Self, Self::BlockBTreeEntry>;
    type BlockTrailer: BlockTrailer<BlockId = Self::BlockId>;
    type AllocationMapPage: AllocationMapPage<Self>;
    type AllocationPageMapPage: AllocationPageMapPage<Self>;
    type FreeMapPage: FreeMapPage<Self>;
    type FreePageMapPage: FreePageMapPage<Self>;
    type DensityListPage: DensityListPage<Self>;
    type DataTreeEntry: IntermediateTreeEntry + IntermediateDataTreeEntry<Self>;
    type DataTreeBlock: IntermediateTreeBlock<
        Header = DataTreeBlockHeader,
        Entry = Self::DataTreeEntry,
        Trailer = Self::BlockTrailer,
    >;
    type DataBlock: Block<Trailer = Self::BlockTrailer>;
    type SubNodeTreeBlockHeader: IntermediateTreeHeader;
    type SubNodeTreeBlock: IntermediateTreeBlock<
        Header = Self::SubNodeTreeBlockHeader,
        Entry = IntermediateSubNodeTreeEntry<Self::BlockId>,
        Trailer = Self::BlockTrailer,
    >;
    type SubNodeBlock: IntermediateTreeBlock<
        Header = Self::SubNodeTreeBlockHeader,
        Entry = LeafSubNodeTreeEntry<Self::BlockId>,
        Trailer = Self::BlockTrailer,
    >;
    type TableContext: TableContext;
    type PropertyContext: PropertyContext;
    type HeapNode: HeapNode;
    type PropertyTree: HeapTree<Key = PropertyTreeRecordKey, Value = PropertyTreeRecordValue>;
    type Store: Store;
    type Folder: Folder;
    type Message: Message;
    type NamedPropertyMap: NamedPropertyMap;
    type SearchUpdateQueue: SearchUpdateQueue;

    fn header(&self) -> &Self::Header;
    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error>;
    fn reader(&self) -> &Mutex<Box<dyn PstReader>>;
    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>>;

    fn read_node(&self, node: NodeId) -> io::Result<Self::NodeBTreeEntry>;
    fn read_block(&self, block: Self::BlockId) -> io::Result<Vec<u8>>;
}

struct PstFileInner<Pst>
where
    Pst: PstFile,
{
    reader: Mutex<Box<dyn PstReader>>,
    writer: PstResult<Mutex<BufWriter<File>>>,
    header: Pst::Header,
    density_list: io::Result<Pst::DensityListPage>,
    node_cache: NodeBTreePageCache<Pst>,
    block_cache: BlockBTreePageCache<Pst>,
}

pub struct UnicodePstFile {
    inner: PstFileInner<Self>,
}

/// A recipient type written to `PR_RECIPIENT_TYPE`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnicodePstRecipientType {
    To = 1,
    Cc = 2,
    Bcc = 3,
}

/// A message recipient for the Unicode PST writer.
#[derive(Clone, Copy)]
pub struct UnicodePstRecipient<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub recipient_type: UnicodePstRecipientType,
    /// Further properties of the recipient, written as given as columns of the message's recipient table. The table's
    /// columns are its fixed ones plus every tag any recipient of the message has; one tag must have one type
    /// throughout. A recipient without a tag has no cell for it.
    pub properties: &'a [UnicodePstProperty<'a>],
}

/// A tagged property given as is: its ID (0x0001 to 0x7FFF) and its value. The writer refuses the IDs it writes itself,
/// and an ID twice in one message or one recipient.
#[derive(Clone, Copy, Debug)]
pub struct UnicodePstProperty<'a> {
    pub id: u16,
    pub value: UnicodePstValue<'a>,
}

/// The value of a [`UnicodePstProperty`]; the variant is the property's type (MS-OXCDATA 2.11.1).
#[derive(Clone, Copy, Debug)]
pub enum UnicodePstValue<'a> {
    /// PtypInteger32.
    Integer32(i32),
    /// PtypBoolean.
    Boolean(bool),
    /// PtypTime, in FILETIME units (100-nanosecond intervals since 1601-01-01 UTC); not negative.
    Time(i64),
    /// PtypString.
    Unicode(&'a str),
    /// PtypBinary.
    Binary(&'a [u8]),
    /// PtypMultipleString.
    MultipleUnicode(&'a [&'a str]),
}

impl UnicodePstValue<'_> {
    fn prop_type(&self) -> PropertyType {
        match self {
            Self::Integer32(_) => PropertyType::Integer32,
            Self::Boolean(_) => PropertyType::Boolean,
            Self::Time(_) => PropertyType::Time,
            Self::Unicode(_) => PropertyType::Unicode,
            Self::Binary(_) => PropertyType::Binary,
            Self::MultipleUnicode(_) => PropertyType::MultipleUnicode,
        }
    }

    fn property_value(&self) -> PropertyValue {
        let unicode = |value: &str| UnicodeValue::new(value.encode_utf16().collect());
        match *self {
            Self::Integer32(value) => PropertyValue::Integer32(value),
            Self::Boolean(value) => PropertyValue::Boolean(value),
            Self::Time(value) => PropertyValue::Time(value),
            Self::Unicode(value) => PropertyValue::Unicode(unicode(value)),
            Self::Binary(value) => PropertyValue::Binary(BinaryValue::new(value.to_vec())),
            Self::MultipleUnicode(values) => {
                PropertyValue::MultipleUnicode(values.iter().map(|value| unicode(value)).collect())
            }
        }
    }

    /// An upper bound of the bytes its value takes in a heap (for `message_data_bound`).
    fn bound(&self) -> u64 {
        let text = |value: &str| 2 * value.len() as u64 + 2;
        match self {
            Self::Integer32(_) | Self::Boolean(_) | Self::Time(_) => 8,
            Self::Unicode(value) => text(value),
            Self::Binary(value) => value.len() as u64,
            Self::MultipleUnicode(values) => {
                4 + values.iter().map(|value| text(value) + 4).sum::<u64>()
            }
        }
    }
}

/// A named property given as is: its property set, its LID or string name, and its value. The writer resolves it
/// through the store's name-to-ID map (MS-PST 2.4.7), adding an entry in the commit that first uses the name. Names
/// match as a store matches them (MS-OXCPRPT, processing of RopGetPropertyIdsFromNames): a LID within its set, a string
/// name exactly, lower-cased first in PS_INTERNET_HEADERS.
#[derive(Clone, Copy, Debug)]
pub struct UnicodePstNamedProperty<'a> {
    /// The property set's GUID in its stored byte order (MS-OXCDATA 2.5.1: Data1, Data2 and Data3 little-endian, then
    /// Data4), as [`guid_bytes`] makes it.
    pub set: [u8; 16],
    pub name: UnicodePstPropertyName<'a>,
    pub value: UnicodePstValue<'a>,
}

/// A named property's name within its set (MS-PST 2.4.7.1: a numerical name or a string).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnicodePstPropertyName<'a> {
    Lid(u32),
    Name(&'a str),
}

/// A GUID in its stored byte order (MS-OXCDATA 2.5.1).
pub const fn guid_bytes(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> [u8; 16] {
    let a = data1.to_le_bytes();
    let b = data2.to_le_bytes();
    let c = data3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], data4[0], data4[1], data4[2], data4[3],
        data4[4], data4[5], data4[6], data4[7],
    ]
}

const OFFICE_GUID_TAIL: [u8; 8] = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];
/// PS_MAPI {00020328-0000-0000-C000-000000000046} (MS-OXPROPS 1.3.2).
pub const PS_MAPI_SET: [u8; 16] = guid_bytes(0x0002_0328, 0, 0, OFFICE_GUID_TAIL);
/// PS_PUBLIC_STRINGS {00020329-0000-0000-C000-000000000046}.
pub const PS_PUBLIC_STRINGS_SET: [u8; 16] = guid_bytes(0x0002_0329, 0, 0, OFFICE_GUID_TAIL);
/// PS_INTERNET_HEADERS {00020386-0000-0000-C000-000000000046}.
pub const PS_INTERNET_HEADERS_SET: [u8; 16] = guid_bytes(0x0002_0386, 0, 0, OFFICE_GUID_TAIL);

/// A named property's name as the map matches it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NameKind {
    Lid(u32),
    /// UTF-16 code units, as the String Stream stores them.
    Name(Vec<u16>),
}

type NameKey = ([u8; 16], NameKind);

fn name_key(set: [u8; 16], name: UnicodePstPropertyName<'_>) -> NameKey {
    let kind = match name {
        UnicodePstPropertyName::Lid(lid) => NameKind::Lid(lid),
        // MS-OXCPRPT: in PS_INTERNET_HEADERS a string name is coerced to lower case before it is matched. Such a name
        // is a header field name, which `check_named_properties` holds to ASCII.
        UnicodePstPropertyName::Name(name) if set == PS_INTERNET_HEADERS_SET => {
            NameKind::Name(name.to_ascii_lowercase().encode_utf16().collect())
        }
        UnicodePstPropertyName::Name(name) => NameKind::Name(name.encode_utf16().collect()),
    };
    (set, kind)
}

/// A stored string name as the map matches it: in PS_INTERNET_HEADERS lower-cased, as `name_key` does with a given one,
/// so a foreign map's "X-Foo" is found for "x-foo" and gets no second entry. Outlook stores them lower-cased already
/// (pstsdk's sample1.pst).
fn stored_name_kind(set: [u8; 16], units: Vec<u16>) -> NameKind {
    match String::from_utf16(&units) {
        Ok(name) if set == PS_INTERNET_HEADERS_SET => {
            name_key(set, UnicodePstPropertyName::Name(&name)).1
        }
        _ => NameKind::Name(units),
    }
}

/// The rules of given named properties, checked before any is resolved: not in PS_MAPI (whose LIDs are tagged property
/// IDs, MS-OXCPRPT), a non-empty name, a header field name in PS_INTERNET_HEADERS (RFC 5322 2.2: printable ASCII but
/// the colon), not the categories' PidNameKeywords (written from `categories`), no name twice, and no negative time.
fn check_named_properties(properties: &[UnicodePstNamedProperty<'_>]) -> io::Result<()> {
    let invalid = |detail: String| io::Error::new(io::ErrorKind::InvalidInput, detail);
    let mut seen = HashSet::new();
    for property in properties {
        let label = match property.name {
            UnicodePstPropertyName::Lid(lid) => format!("LID {lid:#x}"),
            UnicodePstPropertyName::Name(name) => format!("name {name:?}"),
        };
        if property.set == PS_MAPI_SET {
            return Err(invalid(format!(
                "named property {label} in PS_MAPI is a tagged property: give its tag"
            )));
        }
        if let UnicodePstPropertyName::Name(name) = property.name {
            if name.is_empty() {
                return Err(invalid("a named property's name is empty".to_owned()));
            }
            if property.set == PS_INTERNET_HEADERS_SET
                && !name
                    .bytes()
                    .all(|byte| (0x21..=0x7E).contains(&byte) && byte != b':')
            {
                return Err(invalid(format!("named property {label} in PS_INTERNET_HEADERS is not a header field name (RFC 5322)")));
            }
            if property.set == PS_PUBLIC_STRINGS_SET && name == "Keywords" {
                return Err(invalid("named property PS_PUBLIC_STRINGS \"Keywords\" is the categories, which the writer writes itself".to_owned()));
            }
        }
        if !seen.insert(name_key(property.set, property.name)) {
            return Err(invalid(format!("named property {label} is given twice")));
        }
        if matches!(property.value, UnicodePstValue::Time(value) if value < 0) {
            return Err(invalid(format!(
                "named property {label} has a negative time"
            )));
        }
    }
    Ok(())
}

/// The property IDs a commit's named properties have in the store's map: PidNameKeywords for the categories and each
/// given named property.
#[derive(Default)]
pub(crate) struct NameIds {
    keywords: Option<u16>,
    given: HashMap<NameKey, u16>,
}

impl NameIds {
    #[cfg(test)]
    pub(crate) fn with_keywords(keywords: u16) -> Self {
        Self {
            keywords: Some(keywords),
            given: HashMap::new(),
        }
    }

    /// Tests: these named properties' IDs, from 0x8100 on, as a map would give them.
    #[cfg(test)]
    pub(crate) fn with_names(keywords: u16, named: &[UnicodePstNamedProperty<'_>]) -> Self {
        let given = named
            .iter()
            .zip(0x8100_u16..)
            .map(|(property, id)| (name_key(property.set, property.name), id))
            .collect();
        Self {
            keywords: Some(keywords),
            given,
        }
    }

    fn given(&self, property: &UnicodePstNamedProperty<'_>) -> io::Result<u16> {
        self.given
            .get(&name_key(property.set, property.name))
            .copied()
            .ok_or_else(|| {
                io::Error::other("a named property was not resolved through the name-to-ID map")
            })
    }
}

/// The store's name-to-ID map as a writer session holds it (MS-PST 2.4.7): the map's other properties, copied through;
/// the GUID, entry and string streams; the non-empty hash buckets (the Outlook-made template stores no empty one); and
/// an index by name. A session reads it from the file the first time a commit gives named properties, and keeps a
/// commit's additions only once that commit succeeded.
#[derive(Clone)]
pub(crate) struct NameMap {
    others: BTreeMap<u16, PropertyValue>,
    bucket_count: u32,
    guids: Vec<[u8; 16]>,
    entries: Vec<u8>,
    strings: Vec<u8>,
    buckets: BTreeMap<u16, Vec<u8>>,
    index: HashMap<NameKey, u16>,
    next_index: u16,
    changed: bool,
}

impl NameMap {
    fn read(store: &Rc<UnicodeStore>) -> io::Result<Self> {
        let malformed = |detail: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the store's name-to-ID map is malformed: {detail}"),
            )
        };
        let map = UnicodeNamedPropertyMap::read(store.clone())?;
        let (mut others, mut buckets) = (BTreeMap::new(), BTreeMap::new());
        let (mut guids, mut entries, mut strings) = (Vec::new(), Vec::new(), Vec::new());
        let mut bucket_count = None;
        // The properties come in ID order, so PidTagNameidBucketCount (0x0001) is read before any bucket.
        for (id, value) in map.properties().iter() {
            let binary = || match value {
                PropertyValue::Binary(value) => Ok(value.buffer().to_vec()),
                _ => Err(malformed(format!("property {id:#06x} is not binary"))),
            };
            match *id {
                0x0001 => {
                    let PropertyValue::Integer32(count) = value else {
                        return Err(malformed(
                            "PidTagNameidBucketCount is not PtypInteger32".to_owned(),
                        ));
                    };
                    bucket_count = Some(
                        u32::try_from(*count)
                            .ok()
                            .filter(|count| (1..=0xEFFF).contains(count))
                            .ok_or_else(|| malformed(format!("bucket count {count}")))?,
                    );
                    others.insert(*id, value.clone());
                }
                0x0002 => {
                    let data = binary()?;
                    if data.len() % 16 != 0 {
                        return Err(malformed(
                            "the GUID stream is not a whole number of GUIDs".to_owned(),
                        ));
                    }
                    guids = data
                        .chunks(16)
                        .map(|chunk| <[u8; 16]>::try_from(chunk).expect("16 bytes"))
                        .collect();
                }
                0x0003 => entries = binary()?,
                0x0004 => strings = binary()?,
                id if id >= 0x1000
                    && bucket_count.is_some_and(|count| u32::from(id) < 0x1000 + count) =>
                {
                    buckets.insert(id, binary()?);
                }
                id => {
                    others.insert(id, value.clone());
                }
            }
        }
        let bucket_count =
            bucket_count.ok_or_else(|| malformed("no PidTagNameidBucketCount".to_owned()))?;
        if entries.len() % 8 != 0 {
            return Err(malformed(
                "the entry stream is not a whole number of NAMEIDs".to_owned(),
            ));
        }
        let mut index = HashMap::new();
        let mut next_index = 0_u16;
        for record in entries.chunks(8) {
            let dw = u32::from_le_bytes([record[0], record[1], record[2], record[3]]);
            let gw = u16::from_le_bytes([record[4], record[5]]);
            let prop_index = u16::from_le_bytes([record[6], record[7]]);
            if prop_index > 0x7FFE {
                return Err(malformed(format!("wPropIdx {prop_index:#06x}")));
            }
            next_index = next_index.max(prop_index + 1);
            let set = match gw >> 1 {
                // NAMEID_GUID_NONE: a name without a property set, which no given property has.
                0 => continue,
                1 => PS_MAPI_SET,
                2 => PS_PUBLIC_STRINGS_SET,
                w => *guids
                    .get(usize::from(w - 3))
                    .ok_or_else(|| malformed(format!("GUID index {w} is past the GUID stream")))?,
            };
            let kind = if gw & 1 == 0 {
                NameKind::Lid(dw)
            } else {
                let start = usize::try_from(dw).map_err(|_| PstError::IntegerConversion)?;
                let length = strings.get(start..start + 4).map(|bytes| {
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
                });
                let name = length
                    .filter(|length| length % 2 == 0)
                    .and_then(|length| strings.get(start + 4..start + 4 + length))
                    .ok_or_else(|| malformed(format!("string offset {dw:#x}")))?;
                stored_name_kind(
                    set,
                    name.chunks(2)
                        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                        .collect(),
                )
            };
            // A name twice in the map resolves to its first entry, as a lookup that stops at the first match would.
            index.entry((set, kind)).or_insert(0x8000 + prop_index);
        }
        Ok(Self {
            others,
            bucket_count,
            guids,
            entries,
            strings,
            buckets,
            index,
            next_index,
            changed: false,
        })
    }

    /// The property ID of a named property: its entry's, or a new entry's (MS-PST 2.4.7: NAMEID, GUID Stream, String
    /// Stream and hash bucket, laid out as in the Outlook-made template).
    fn resolve(&mut self, set: [u8; 16], name: UnicodePstPropertyName<'_>) -> io::Result<u16> {
        let key = name_key(set, name);
        if let Some(id) = self.index.get(&key) {
            return Ok(*id);
        }
        let full = |what: &str| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the store's name-to-ID map is full: {what}"),
            )
        };
        let prop_index = self.next_index;
        if prop_index > 0x7FFE {
            return Err(full("no property ID is left from 0x8000 to 0xFFFE"));
        }
        let guid_index = if set == PS_PUBLIC_STRINGS_SET {
            2
        } else {
            let position = match self.guids.iter().position(|guid| *guid == set) {
                Some(position) => position,
                None => {
                    self.guids.push(set);
                    self.guids.len() - 1
                }
            };
            u16::try_from(position + 3)
                .ok()
                .filter(|index| *index <= 0x7FFF)
                .ok_or_else(|| full("the GUID stream has no index left"))?
        };
        let (dw, gw, hash) = match &key.1 {
            NameKind::Lid(lid) => (*lid, guid_index << 1, *lid),
            NameKind::Name(units) => {
                // MS-PST 2.4.7.4: each length/string pair ends on a 4-byte boundary, the padding inside the stream.
                self.strings
                    .resize(self.strings.len().next_multiple_of(4), 0);
                let offset = u32::try_from(self.strings.len())
                    .map_err(|_| full("the string stream passes 4 GiB"))?;
                let bytes = units
                    .iter()
                    .flat_map(|unit| unit.to_le_bytes())
                    .collect::<Vec<_>>();
                self.strings.extend(
                    u32::try_from(bytes.len())
                        .map_err(|_| PstError::IntegerConversion)?
                        .to_le_bytes(),
                );
                self.strings.extend(&bytes);
                self.strings
                    .resize(self.strings.len().next_multiple_of(4), 0);
                // A bucket record holds the name's CRC (MS-PST 5.3, as the template's string records do).
                (
                    offset,
                    (guid_index << 1) | 1,
                    crate::crc::compute_crc(0, &bytes),
                )
            }
        };
        let record = |dw: u32| {
            let mut record = dw.to_le_bytes().to_vec();
            record.extend(gw.to_le_bytes());
            record.extend(prop_index.to_le_bytes());
            record
        };
        self.entries.extend(record(dw));
        let bucket = 0x1000
            + u16::try_from((hash ^ u32::from(gw)) % self.bucket_count)
                .map_err(|_| PstError::IntegerConversion)?;
        self.buckets.entry(bucket).or_default().extend(record(hash));
        let id = 0x8000 + prop_index;
        self.index.insert(key, id);
        self.next_index = prop_index + 1;
        self.changed = true;
        Ok(id)
    }

    /// The map's property context, in ID order: its other properties as they were, the three streams and the
    /// non-empty buckets.
    fn properties(&self) -> Vec<(u16, PropertyValue)> {
        let binary = |data: Vec<u8>| PropertyValue::Binary(BinaryValue::new(data));
        let mut properties = self.others.clone();
        properties.insert(0x0002, binary(self.guids.concat()));
        properties.insert(0x0003, binary(self.entries.clone()));
        properties.insert(0x0004, binary(self.strings.clone()));
        for (id, data) in &self.buckets {
            properties.insert(*id, binary(data.clone()));
        }
        properties.into_iter().collect()
    }
}

/// The property IDs the writer writes in a message's property context or computes in its contents-table row; a given
/// property may not have one of them.
pub const MESSAGE_OWNED_IDS: &[u16] = &[
    // 0x003D, PidTagSubjectPrefix, is not written: MS-PST 2.5.3.1 calculates it, and PidTagNormalizedSubject (0x0E1D),
    // from PidTagSubject, so neither may be given.
    0x0017, 0x001A, 0x0037, 0x0039, 0x003D, 0x0042, 0x0064, 0x0065, 0x0070, 0x007D, 0x0C1A, 0x0C1E,
    0x0C1F, 0x0E02, 0x0E03, 0x0E04, 0x0E06, 0x0E07, 0x0E08, 0x0E09, 0x0E12, 0x0E13, 0x0E17, 0x0E1B,
    0x0E1D, 0x0FFF, 0x1000, 0x1013, 0x1035, 0x3001, 0x3007, 0x3008, 0x300B, 0x3013, 0x3FDE, 0x67F2,
    0x67F3,
];

/// The columns the writer writes in every recipient row.
pub const RECIPIENT_OWNED_IDS: &[u16] = &[
    0x0C15, 0x0E0F, 0x0FF9, 0x0FFE, 0x0FFF, 0x3001, 0x3002, 0x3003, 0x300B, 0x3900, 0x39FE, 0x39FF,
    0x3A40, 0x67F2, 0x67F3,
];

/// The rules of given properties: IDs 0x0001 to 0x7FFF, none the writer owns, none twice, times not negative.
fn check_properties(
    properties: &[UnicodePstProperty<'_>],
    owned: &[u16],
    what: &str,
) -> io::Result<()> {
    let invalid = |detail: String| io::Error::new(io::ErrorKind::InvalidInput, detail);
    let mut seen = std::collections::BTreeSet::new();
    for property in properties {
        if property.id == 0 || property.id >= 0x8000 {
            return Err(invalid(format!(
                "{what} property {:#06x}: only tagged properties (0x0001 to 0x7FFF) are written",
                property.id
            )));
        }
        if owned.contains(&property.id) {
            return Err(invalid(format!(
                "{what} property {:#06x} is one the writer writes itself",
                property.id
            )));
        }
        if !seen.insert(property.id) {
            return Err(invalid(format!(
                "{what} property {:#06x} is given twice",
                property.id
            )));
        }
        if matches!(property.value, UnicodePstValue::Time(value) if value < 0) {
            return Err(invalid(format!(
                "{what} property {:#06x} has a negative time",
                property.id
            )));
        }
    }
    Ok(())
}

/// `PidTagImportance` of a message. `PidTagPriority` is not derived from it: the two properties
/// are distinct (transport order versus user-visible importance) and no rule maps one to the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnicodePstImportance {
    Low,
    Normal,
    High,
}

impl UnicodePstImportance {
    fn importance(self) -> i32 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::High => 2,
        }
    }
}

/// A message's sender: `PidTagSenderName`, `PidTagSenderEmailAddress` and `PidTagSenderAddressType`, mirrored into the
/// sent-representing properties because this writer has no delegate input.
#[derive(Clone, Copy, Debug)]
pub struct UnicodePstSender<'a> {
    pub name: &'a str,
    pub email: &'a str,
    /// For example `SMTP`.
    pub address_type: &'a str,
}

/// Message input for the Unicode PST writer.
#[derive(Clone, Copy)]
pub struct UnicodePstMessage<'a> {
    /// `PidTagMessageClass` (0x001A), written as given in the message and in its contents-table row. MS-OXCMSG 2.2.1.3:
    /// ASCII 0x20 to 0x7F, not ending with a period, 1 to 255 characters; anything else is refused as invalid input.
    /// Matching a class is case-insensitive; this writer does not match, it writes.
    pub message_class: &'a str,
    pub subject: &'a str,
    /// The sender, written as both the sender and the sent-representing identity. `None` writes none of their six
    /// properties (0x0042, 0x0064, 0x0065, 0x0C1A, 0x0C1E, 0x0C1F), as for a draft that has no sender yet.
    pub sender: Option<UnicodePstSender<'a>>,
    pub recipients: &'a [UnicodePstRecipient<'a>],
    pub body: &'a str,
    /// UTF-8 HTML for `PR_HTML`; `body` remains the plain-text fallback.
    pub html_body: Option<&'a str>,
    /// `PidTagInternetMessageId` (0x1035); `None` leaves it unwritten. A message without a `search_key` needs one.
    pub message_id: Option<&'a str>,
    /// `PidTagMessageDeliveryTime` (0x0E06): number of 100-nanosecond intervals since 1601-01-01 UTC. `None` leaves it
    /// unwritten, as for a draft.
    pub delivery_time: Option<i64>,
    /// `PidTagCreationTime` (0x3007), FILETIME units.
    pub creation_time: i64,
    /// `PidTagLastModificationTime` (0x3008), FILETIME units, in the message and its contents-table row.
    pub last_modification_time: i64,
    /// `mfUnsent` (0x00000008) of `PidTagMessageFlags`: the message is still being composed and is treated as a draft
    /// (MS-OXCMSG 2.2.1.6).
    pub unsent: bool,
    /// Further tagged properties, written as given in the message's property context, and in its contents-table row
    /// wherever the folder's contents table has that column with that type. They come after the writer's own, so a
    /// message without any is unchanged byte for byte.
    pub properties: &'a [UnicodePstProperty<'a>],
    /// Named properties, written as given: each under the ID the store's name-to-ID map gives its name, the map gaining
    /// an entry in the commit that first uses a name; in the property context after the tagged ones, and in the
    /// contents-table row wherever the folder's contents table has that ID with that type.
    pub named_properties: &'a [UnicodePstNamedProperty<'a>],
    /// Optional `PR_SEARCH_KEY` bytes identifying this export item. `None` keeps the legacy
    /// behaviour of deriving the key from `message_id`, which is not unique when two distinct
    /// items share one Internet Message-ID.
    pub search_key: Option<&'a [u8]>,
    /// `MSGFLAG_READ` of `PidTagMessageFlags`; `MSGFLAG_HASATTACH` is derived from the attachments.
    pub is_read: bool,
    /// `PidTagImportance`. `PidTagPriority` is not written (there is no input for it).
    pub importance: UnicodePstImportance,
    /// `PidTagClientSubmitTime` in FILETIME units; `None` leaves the property unwritten.
    pub client_submit_time: Option<i64>,
    /// `PidNameKeywords` (categories), written in this order as `PtypMultipleString` under the property ID that the
    /// store's own name-to-id map gives PS_PUBLIC_STRINGS "Keywords". An empty slice writes nothing. The writer adds no
    /// map entries: a store whose map lacks "Keywords" makes the append fail.
    pub categories: &'a [&'a str],
    /// `PidTagTransportMessageHeaders` (0x007D, PtypString; MS-OXOMSG 2.2.1.61), written as given, an empty value
    /// included; `None` leaves the property unwritten.
    pub transport_headers: Option<&'a str>,
}

/// Streamed attachment content: the declared length and a one-shot source. The writer calls `open` once, while it
/// writes this attachment, reads exactly `len` bytes in data blocks (at most 128 of them are held before they go to the
/// file) and then requires the end of the stream. A stream that ends early or has more bytes, or any read error, makes
/// the append fail before its commit. Whatever the source checks (a size or a digest at the end of the stream) is
/// therefore checked on exactly the bytes written.
#[derive(Clone, Copy)]
pub struct UnicodePstAttachmentStream<'a> {
    /// Declared length in bytes.
    pub len: u64,
    /// Opens the content; called once per append that writes the attachment.
    pub open: &'a dyn Fn() -> io::Result<Box<dyn Read>>,
}

/// A message the PST format cannot hold: its stored size, or one attachment object's, does not fit PidTagMessageSize or
/// PidTagAttachSize, both PtypInteger32 (MS-PST 2.4.5.1.1 and 2.4.6.1.1); a single data tree is also bounded by its
/// 32-bit lcbTotal (2.2.2.8.3.2.1). The append that meets it fails as an `io::ErrorKind::FileTooLarge` error carrying
/// this value, before any block of its commit is written.
#[derive(Debug, Clone, Copy)]
pub struct UnicodePstMessageTooLarge {
    /// The message's 1-based position in the append call; 0 when not known yet.
    pub position: usize,
    /// The size the writer computed for the message or the attachment object, in bytes.
    pub size: u64,
}

impl std::fmt::Display for UnicodePstMessageTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "message {} is {} bytes; PidTagMessageSize and PidTagAttachSize are 32-bit signed integers (MS-PST 2.4.5.1.1, \
             2.4.6.1.1), so a PST cannot hold it",
            self.position, self.size
        )
    }
}

impl std::error::Error for UnicodePstMessageTooLarge {}

/// An append that failed while preparing or writing one message, with that message's 1-based position in the append
/// call, so that a caller can name the message's item. Its text is the writer's message ("append message N of M
/// (Message-ID) at PST size S: cause"); the error kind is the cause's (for example StorageFull).
#[derive(Debug)]
pub struct UnicodePstAppendFailed {
    /// The message's 1-based position in the append call.
    pub position: usize,
    /// The writer's description, the position and the cause included.
    pub detail: String,
}

impl std::fmt::Display for UnicodePstAppendFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for UnicodePstAppendFailed {}

/// A commit would have left the file larger than the session's limit (`UnicodePstWriter::set_max_file_size`). The
/// commit was abandoned before its header, and the file cut back to its last commit. The append fails as an
/// `io::ErrorKind::FileTooLarge` error carrying this value.
#[derive(Debug, Clone, Copy)]
pub struct UnicodePstFileLimitReached {
    /// The file size the commit would have left, in bytes.
    pub size: u64,
    /// The session's limit, in bytes.
    pub limit: u64,
}

impl std::fmt::Display for UnicodePstFileLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the commit would leave the file at {} bytes, past its limit of {} bytes; it was not committed", self.size, self.limit)
    }
}

impl std::error::Error for UnicodePstFileLimitReached {}

fn too_large(size: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        UnicodePstMessageTooLarge {
            position: 0,
            size: size as u64,
        },
    )
}

/// A by-value file attachment for the Unicode PST writer.
pub struct UnicodePstAttachment<'a> {
    /// File name shown by mail clients.
    pub filename: &'a str,
    /// MIME content type, such as `application/pdf`.
    pub mime_type: &'a str,
    /// MIME Content-ID for an inline HTML attachment. Surrounding `<` and `>` are removed.
    /// The HTML body must reference the normalized value with the `cid:` scheme.
    pub content_id: Option<&'a str>,
    /// Exact attachment bytes (empty when `stream` is used).
    pub data: &'a [u8],
    /// Streamed content instead of `data`; see [`UnicodePstAttachmentStream`].
    pub stream: Option<UnicodePstAttachmentStream<'a>>,
    /// `PidTagAttachmentHidden`: `true` for inline parts that mail clients should not list. With a `content_id` it also
    /// sets PidTagAttachFlags' ATT_MHTML_REF (0x4), the inline mark; a listed attachment keeps its Content-ID without it.
    pub hidden: bool,
    /// An attached e-mail. When set, the attachment is that message, stored as PidTagAttachMethod 5 with
    /// PidTagAttachDataObject (MS-PST 2.4.6.2.2): `filename` is written as PidTagDisplayName, `mime_type` is not
    /// written, and `data` must be empty, `stream` and `content_id` unset.
    pub attached_message: Option<Box<UnicodePstAttachedMessage<'a>>>,
}

/// An attached e-mail: a message and its own attachments, which may be attached e-mails again. Held by value, so that a
/// caller can build a nested tree from data it owns.
pub struct UnicodePstAttachedMessage<'a> {
    pub message: UnicodePstMessage<'a>,
    pub attachments: Vec<UnicodePstAttachment<'a>>,
}

/// The content length of an attachment: its stream's declared length, or its bytes.
fn attachment_len(attachment: &UnicodePstAttachment<'_>) -> io::Result<usize> {
    match attachment.stream {
        Some(_) if !attachment.data.is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an attachment has either bytes or a stream, not both",
        )),
        Some(stream) => usize::try_from(stream.len).map_err(|_| PstError::IntegerConversion.into()),
        None => Ok(attachment.data.len()),
    }
}

/// One message and its attachments for a bulk append operation.
#[derive(Clone, Copy)]
pub struct UnicodePstBatchMessage<'a> {
    pub message: UnicodePstMessage<'a>,
    pub attachments: &'a [UnicodePstAttachment<'a>],
}

impl UnicodePstFile {
    pub fn read_from(reader: Box<dyn PstReader>) -> io::Result<Self> {
        let inner = PstFileInner::read_from(reader)?;
        Ok(Self { inner })
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let inner = PstFileInner::open(path)?;
        Ok(Self { inner })
    }

    /// Create a new Unicode PST containing Inbox and one message.
    pub fn create(path: impl AsRef<Path>, message: &UnicodePstMessage<'_>) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, &[], None)?;
        Self::open(path)
    }

    /// Create a new Unicode PST containing one message and its by-value attachments.
    pub fn create_with_attachments(
        path: impl AsRef<Path>,
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, attachments, None)?;
        Self::open(path)
    }

    /// Create a new Unicode PST and place its first message in `folder_path`, relative to the
    /// IPM subtree. Missing folders are created; for example `&["Projects", "2026"]`.
    pub fn create_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, &[], Some(folder_path))?;
        Self::open(path)
    }

    /// Create a new Unicode PST and place its first message and attachments in `folder_path`.
    pub fn create_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        create_unicode_pst(path.as_ref(), message, attachments, Some(folder_path))?;
        Self::open(path)
    }

    /// Set the store display name (PR_DISPLAY_NAME of the message store) of an existing
    /// unencrypted Unicode PST. Outlook shows it as the top-level name when the file is opened.
    pub fn set_store_display_name(path: impl AsRef<Path>, name: &str) -> io::Result<Self> {
        set_unicode_store_display_name(&mut WriterCore::open(path.as_ref())?, name)?;
        Self::open(path)
    }

    /// Append one message to the default receive folder (normally Inbox) of an
    /// existing unencrypted Unicode PST.
    pub fn append(path: impl AsRef<Path>, message: &UnicodePstMessage<'_>) -> io::Result<Self> {
        append_unicode_pst_at(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments: &[],
            }],
            None,
        )?;
        Self::open(path)
    }

    /// Append one message and its by-value attachments to the default receive folder of an
    /// existing unencrypted Unicode PST.
    pub fn append_with_attachments(
        path: impl AsRef<Path>,
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst_at(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments,
            }],
            None,
        )?;
        Self::open(path)
    }

    /// Append messages to the default receive folder in one bulk commit.
    pub fn append_many(
        path: impl AsRef<Path>,
        messages: &[UnicodePstMessage<'_>],
    ) -> io::Result<Self> {
        let messages = messages
            .iter()
            .copied()
            .map(|message| UnicodePstBatchMessage {
                message,
                attachments: &[],
            })
            .collect::<Vec<_>>();
        append_unicode_pst_at(path.as_ref(), &messages, None)?;
        Self::open(path)
    }

    /// Append messages and attachments to the default receive folder in one bulk commit. Message
    /// and attachment blocks are written incrementally instead of retained for the whole batch.
    pub fn append_many_with_attachments(
        path: impl AsRef<Path>,
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst_at(path.as_ref(), messages, None)?;
        Self::open(path)
    }

    /// Append one message to `folder_path`, relative to the IPM subtree. Missing folders are
    /// created and existing folder contents are preserved.
    pub fn append_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
    ) -> io::Result<Self> {
        append_unicode_pst_at(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments: &[],
            }],
            Some(folder_path),
        )?;
        Self::open(path)
    }

    /// Append one message and its attachments to `folder_path`, creating missing folders.
    pub fn append_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        message: &UnicodePstMessage<'_>,
        attachments: &[UnicodePstAttachment<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst_at(
            path.as_ref(),
            &[UnicodePstBatchMessage {
                message: *message,
                attachments,
            }],
            Some(folder_path),
        )?;
        Self::open(path)
    }

    /// Append messages to `folder_path` in one bulk commit, creating missing folders.
    pub fn append_many_in_folder(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        messages: &[UnicodePstMessage<'_>],
    ) -> io::Result<Self> {
        let messages = messages
            .iter()
            .copied()
            .map(|message| UnicodePstBatchMessage {
                message,
                attachments: &[],
            })
            .collect::<Vec<_>>();
        append_unicode_pst_at(path.as_ref(), &messages, Some(folder_path))?;
        Self::open(path)
    }

    /// Append messages and attachments to `folder_path` in one bulk commit, creating missing
    /// folders. Message and attachment blocks are written incrementally.
    pub fn append_many_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<Self> {
        append_unicode_pst_at(path.as_ref(), messages, Some(folder_path))?;
        Self::open(path)
    }

    /// Same commit as [`Self::append_many_in_folder_with_attachments`] without opening the file again afterwards: a
    /// caller that would drop the returned handle saves two opens and a header and density-list read per commit.
    pub fn commit_many_in_folder_with_attachments(
        path: impl AsRef<Path>,
        folder_path: &[&str],
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<()> {
        append_unicode_pst_at(path.as_ref(), messages, Some(folder_path))
    }
}

impl PstFileLock<UnicodePstFile> for UnicodePstFile {
    fn start_write(&mut self) -> io::Result<()> {
        self.inner.start_write()
    }

    fn finish_write(&mut self) -> io::Result<()> {
        self.inner.finish_write()
    }

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::BlockBTree>> {
        self.inner.block_cache.borrow_mut()
    }

    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::NodeBTree>> {
        self.inner.node_cache.borrow_mut()
    }
}

impl PstFile for UnicodePstFile {
    type BlockId = UnicodeBlockId;
    type PageId = UnicodePageId;
    type ByteIndex = UnicodeByteIndex;
    type BlockRef = UnicodeBlockRef;
    type PageRef = UnicodePageRef;
    type Root = UnicodeRoot;
    type Header = UnicodeHeader;
    type PageTrailer = UnicodePageTrailer;
    type BTreeKey = u64;
    type NodeBTreeEntry = UnicodeNodeBTreeEntry;
    type NodeBTree = UnicodeNodeBTree;
    type BlockBTreeEntry = UnicodeBlockBTreeEntry;
    type BlockBTree = UnicodeBlockBTree;
    type BlockTrailer = UnicodeBlockTrailer;
    type AllocationMapPage = UnicodeMapPage<{ PageType::AllocationMap as u8 }>;
    type AllocationPageMapPage = UnicodeMapPage<{ PageType::AllocationPageMap as u8 }>;
    type FreeMapPage = UnicodeMapPage<{ PageType::FreeMap as u8 }>;
    type FreePageMapPage = UnicodeMapPage<{ PageType::FreePageMap as u8 }>;
    type DensityListPage = UnicodeDensityListPage;
    type DataTreeEntry = UnicodeDataTreeEntry;
    type DataTreeBlock = UnicodeDataTreeBlock;
    type DataBlock = UnicodeDataBlock;
    type SubNodeTreeBlockHeader = UnicodeSubNodeTreeBlockHeader;
    type SubNodeTreeBlock = UnicodeIntermediateSubNodeTreeBlock;
    type SubNodeBlock = UnicodeLeafSubNodeTreeBlock;
    type HeapNode = UnicodeHeapNode;
    type PropertyTree = UnicodeHeapTree<PropertyTreeRecordKey, PropertyTreeRecordValue>;
    type TableContext = UnicodeTableContext;
    type PropertyContext = UnicodePropertyContext;
    type Store = UnicodeStore;
    type Folder = UnicodeFolder;
    type Message = UnicodeMessage;
    type NamedPropertyMap = UnicodeNamedPropertyMap;
    type SearchUpdateQueue = UnicodeSearchUpdateQueue;

    fn header(&self) -> &Self::Header {
        &self.inner.header
    }

    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error> {
        self.inner.density_list.as_ref().map(|dl| dl as _)
    }

    fn reader(&self) -> &Mutex<Box<dyn PstReader>> {
        &self.inner.reader
    }

    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>> {
        PstFileLockGuard::new(self)
    }

    fn read_node(&self, node: NodeId) -> io::Result<UnicodeNodeBTreeEntry> {
        self.inner.read_node(node)
    }

    fn read_block(&self, block: UnicodeBlockId) -> io::Result<Vec<u8>> {
        self.inner.read_block(block)
    }
}

pub struct AnsiPstFile {
    inner: PstFileInner<Self>,
}

impl AnsiPstFile {
    pub fn read_from(reader: Box<dyn PstReader>) -> io::Result<Self> {
        let inner = PstFileInner::read_from(reader)?;
        Ok(Self { inner })
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let inner = PstFileInner::open(path)?;
        Ok(Self { inner })
    }
}

impl PstFileLock<AnsiPstFile> for AnsiPstFile {
    fn start_write(&mut self) -> io::Result<()> {
        self.inner.start_write()
    }

    fn finish_write(&mut self) -> io::Result<()> {
        self.inner.finish_write()
    }

    fn block_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::BlockBTree>> {
        self.inner.block_cache.borrow_mut()
    }

    fn node_cache(&self) -> RefMut<'_, RootBTreePageCache<<Self as PstFile>::NodeBTree>> {
        self.inner.node_cache.borrow_mut()
    }
}

impl PstFile for AnsiPstFile {
    type BlockId = AnsiBlockId;
    type PageId = AnsiPageId;
    type ByteIndex = AnsiByteIndex;
    type BlockRef = AnsiBlockRef;
    type PageRef = AnsiPageRef;
    type Root = AnsiRoot;
    type Header = AnsiHeader;
    type PageTrailer = AnsiPageTrailer;
    type BTreeKey = u32;
    type NodeBTreeEntry = AnsiNodeBTreeEntry;
    type NodeBTree = AnsiNodeBTree;
    type BlockBTreeEntry = AnsiBlockBTreeEntry;
    type BlockBTree = AnsiBlockBTree;
    type BlockTrailer = AnsiBlockTrailer;
    type AllocationMapPage = AnsiMapPage<{ PageType::AllocationMap as u8 }>;
    type AllocationPageMapPage = AnsiMapPage<{ PageType::AllocationPageMap as u8 }>;
    type FreeMapPage = AnsiMapPage<{ PageType::FreeMap as u8 }>;
    type FreePageMapPage = AnsiMapPage<{ PageType::FreePageMap as u8 }>;
    type DensityListPage = AnsiDensityListPage;
    type DataTreeEntry = AnsiDataTreeEntry;
    type DataTreeBlock = AnsiDataTreeBlock;
    type DataBlock = AnsiDataBlock;
    type SubNodeTreeBlockHeader = AnsiSubNodeTreeBlockHeader;
    type SubNodeTreeBlock = AnsiIntermediateSubNodeTreeBlock;
    type SubNodeBlock = AnsiLeafSubNodeTreeBlock;
    type HeapNode = AnsiHeapNode;
    type PropertyTree = AnsiHeapTree<PropertyTreeRecordKey, PropertyTreeRecordValue>;
    type TableContext = AnsiTableContext;
    type PropertyContext = AnsiPropertyContext;
    type Store = AnsiStore;
    type Folder = AnsiFolder;
    type Message = AnsiMessage;
    type NamedPropertyMap = AnsiNamedPropertyMap;
    type SearchUpdateQueue = AnsiSearchUpdateQueue;

    fn header(&self) -> &Self::Header {
        &self.inner.header
    }

    fn density_list(&self) -> Result<&dyn DensityListPage<Self>, &io::Error> {
        self.inner.density_list.as_ref().map(|dl| dl as _)
    }

    fn reader(&self) -> &Mutex<Box<dyn PstReader>> {
        &self.inner.reader
    }

    fn lock(&mut self) -> io::Result<PstFileLockGuard<'_, Self>> {
        PstFileLockGuard::new(self)
    }

    fn read_node(&self, node: NodeId) -> io::Result<AnsiNodeBTreeEntry> {
        self.inner.read_node(node)
    }

    fn read_block(&self, block: AnsiBlockId) -> io::Result<Vec<u8>> {
        self.inner.read_block(block)
    }
}

const AMAP_FIRST_OFFSET: u64 = 0x4400;
const AMAP_DATA_SIZE: u64 = size_of::<MapBits>() as u64 * 8 * 64;

const PMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + PAGE_SIZE as u64;
const PMAP_PAGE_COUNT: u64 = 8;
const PMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * PMAP_PAGE_COUNT;

const FMAP_FIRST_SIZE: u64 = 128;
const FMAP_FIRST_DATA_SIZE: u64 = AMAP_DATA_SIZE * FMAP_FIRST_SIZE;
const FMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + FMAP_FIRST_DATA_SIZE + (2 * PAGE_SIZE) as u64;
const FMAP_PAGE_COUNT: u64 = size_of::<MapBits>() as u64;
const FMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * FMAP_PAGE_COUNT;

const FPMAP_FIRST_SIZE: u64 = 128 * 64;
const FPMAP_FIRST_DATA_SIZE: u64 = AMAP_DATA_SIZE * FPMAP_FIRST_SIZE;
const FPMAP_FIRST_OFFSET: u64 = AMAP_FIRST_OFFSET + FPMAP_FIRST_DATA_SIZE + (2 * PAGE_SIZE) as u64;
const FPMAP_PAGE_COUNT: u64 = size_of::<MapBits>() as u64 * 64;
const FPMAP_DATA_SIZE: u64 = AMAP_DATA_SIZE * FPMAP_PAGE_COUNT;

const MAX_DATA_BLOCK_SIZE: usize =
    (ndb::block::MAX_BLOCK_SIZE - UnicodeBlockTrailer::SIZE) as usize;
// MS-PST limit for one HN allocation. Outlook never writes above it; SCANPST stops on padding above it (Fatal Error 80040813). Larger values go to subnodes and BTH nodes are split to stay below it.
const MAX_HN_ALLOCATION_SIZE: usize = 3580;

const OUTLOOK_UNICODE_PST_TEMPLATE: &[u8] = include_bytes!("../examples/Empty.pst");

/// The NID type of every subnode the LTP layer creates: a table's row matrix, a property value too large for its heap,
/// an attachment's data. Outlook-written stores use NID_TYPE_LTP (0x1F) for these. With NID_TYPE_INTERNAL (0x01) the
/// first row matrix, 0x21, would also be NID_MESSAGE_STORE, and libpff takes that row block for an encrypted table and
/// misreads it.
const LTP_SUBNODE: NodeIdType = NodeIdType::ListsTablesProperties;

fn heap_id(allocation_index: usize) -> io::Result<HeapId> {
    Ok(HeapId::new(
        u16::try_from(allocation_index).map_err(|_| PstError::IntegerConversion)?,
        0,
    )?)
}

/// A string value as a PST stores it: UTF-16LE without a terminating NUL. Outlook-written stores hold no terminator: 0
/// of 205 string values and cells in two Outlook-written samples, 0 of 12 in the template.
fn utf16_bytes(value: &str) -> Vec<u8> {
    value.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// UTF-16LE with a terminating NUL, for structures whose format requires one (the strings of a one-off entry ID,
/// MS-OXCDATA 2.2.5.1).
fn utf16z_bytes(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn finish_heap(
    mut allocations: Vec<Vec<u8>>,
    client_signature: HeapNodeType,
    user_root: HeapId,
) -> io::Result<Vec<u8>> {
    const HEAP_HEADER_SIZE: usize = 12;
    let page_map_offset =
        (allocations.iter().map(Vec::len).sum::<usize>() + HEAP_HEADER_SIZE).next_multiple_of(2);
    let page_map_offset =
        u16::try_from(page_map_offset).map_err(|_| PstError::IntegerConversion)?;
    let header = HeapNodeHeader::new(
        page_map_offset,
        client_signature,
        user_root,
        [HeapFillLevel::Empty; 8],
    );
    header.write(&mut allocations[0])?;

    let mut data = allocations.remove(0);
    let mut offsets = Vec::with_capacity(allocations.len() + 1);
    offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    for allocation in allocations {
        data.extend_from_slice(&allocation);
        offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    }

    data.resize(usize::from(page_map_offset), 0);
    data.write_u16::<byteorder::LittleEndian>(
        u16::try_from(offsets.len() - 1).map_err(|_| PstError::IntegerConversion)?,
    )?;
    data.write_u16::<byteorder::LittleEndian>(0)?;
    for offset in offsets {
        data.write_u16::<byteorder::LittleEndian>(offset)?;
    }
    // The fill level of the heap's only block (the other seven are absent, so zero).
    let level = fill_level(MAX_DATA_BLOCK_SIZE.saturating_sub(data.len()));
    write_fill_levels(0, &mut data, 1, |_| Ok(level))?;
    Ok(data)
}

/// The fill level of a heap block with `free` bytes left after its page map (MS-PST 2.3.1.2).
fn fill_level(free: usize) -> HeapFillLevel {
    use HeapFillLevel::*;
    match free {
        3584.. => Empty,
        2560.. => Level1,
        2048.. => Level2,
        1792.. => Level3,
        1536.. => Level4,
        1280.. => Level5,
        1024.. => Level6,
        768.. => Level7,
        512.. => Level8,
        256.. => Level9,
        128.. => Level10,
        64.. => Level11,
        32.. => Level12,
        16.. => Level13,
        8.. => Level14,
        _ => Level15,
    }
}

/// The page whose header stores page `page`'s fill level: HNHDR (page 0) for pages 0-7, else the HNBITMAPHDR of its
/// group of 128 (pages 8, 136, ...).
fn fill_level_page(page: usize) -> usize {
    if page < 8 {
        0
    } else {
        8 + (page - 8) / 128 * 128
    }
}

/// Write the fill levels a header page (`page` 0 or a bitmap page) stores into its encoded bytes: one nibble per
/// block, the first block in the low nibble of the first byte; zero for blocks the heap does not have.
fn write_fill_levels(
    page: usize,
    data: &mut [u8],
    count: usize,
    level: impl Fn(usize) -> io::Result<HeapFillLevel>,
) -> io::Result<()> {
    let (offset, slots) = if page == 0 { (8, 8) } else { (2, 128) };
    for slot in 0..slots {
        let target = page + slot;
        let value = if target < count {
            level(target)? as u8
        } else {
            0
        };
        let byte = data.get_mut(offset + slot / 2).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "heap header too short for its fill levels",
            )
        })?;
        let shift = 4 * (slot % 2);
        *byte = (*byte & !(0x0F << shift)) | (value << shift);
    }
    Ok(())
}

/// The fill levels a header page stores, as `write_fill_levels` writes them: (page, level).
fn read_fill_levels(page: usize, data: &[u8]) -> io::Result<Vec<(usize, HeapFillLevel)>> {
    let (offset, slots) = match page {
        0 => (8, 8),
        bitmap if bitmap % 128 == 8 => (2, 128),
        _ => return Ok(Vec::new()),
    };
    (0..slots)
        .map(|slot| {
            let byte = data.get(offset + slot / 2).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "heap header too short for its fill levels",
                )
            })?;
            let level = HeapFillLevel::try_from((byte >> (4 * (slot % 2))) & 0x0F)
                .map_err(io::Error::from)?;
            Ok((page + slot, level))
        })
        .collect()
}

/// A heap-on-node being built or changed. A builder from `new` or `from_pages` holds every page. One from `paged` holds
/// only the pages a contents-table append loaded from disk, so an append keeps the pages it touches in memory, not the
/// whole heap.
struct HeapBuilder {
    /// Loaded pages by page index; index 0 of a page's vector is its header slot.
    pages: BTreeMap<usize, Vec<Vec<u8>>>,
    /// The heap's number of pages, loaded or not.
    count: usize,
    /// Pages whose encoding changed; only these are encoded by `encode_dirty`.
    dirty: BTreeSet<usize>,
    /// The fill levels stored in the headers of loaded header pages (page 0, bitmap pages), for pages that are not
    /// encoded again: an unchanged page keeps its level.
    stored_levels: BTreeMap<usize, HeapFillLevel>,
}

impl HeapBuilder {
    fn new() -> Self {
        Self {
            pages: BTreeMap::from([(0, vec![Vec::new()])]),
            count: 1,
            dirty: BTreeSet::from([0]),
            stored_levels: BTreeMap::new(),
        }
    }

    fn from_pages(pages: Vec<Vec<Vec<u8>>>) -> Self {
        let count = pages.len();
        Self {
            pages: pages.into_iter().enumerate().collect(),
            count,
            dirty: BTreeSet::new(),
            stored_levels: BTreeMap::new(),
        }
    }

    /// A heap of `count` pages, none of them loaded.
    fn paged(count: usize) -> Self {
        Self {
            pages: BTreeMap::new(),
            count,
            dirty: BTreeSet::new(),
            stored_levels: BTreeMap::new(),
        }
    }

    fn is_loaded(&self, page: usize) -> bool {
        self.pages.contains_key(&page)
    }

    /// Add a page read from disk, as `decode_heap_page` returns it. A page already loaded is kept as it is.
    fn load(&mut self, page: usize, allocations: Vec<Vec<u8>>) {
        self.pages.entry(page).or_insert(allocations);
    }

    /// Record the fill levels a header page read from disk stores.
    fn load_levels(&mut self, page: usize, data: &[u8]) -> io::Result<()> {
        for (target, level) in read_fill_levels(page, data)? {
            self.stored_levels.entry(target).or_insert(level);
        }
        Ok(())
    }

    /// The header pages that store the fill levels of the dirty pages; `encode_dirty` encodes them too, so each must be
    /// loaded.
    fn level_pages(&self) -> BTreeSet<usize> {
        self.dirty
            .iter()
            .map(|&page| fill_level_page(page))
            .collect()
    }

    fn page_mut(&mut self, page: usize) -> io::Result<&mut Vec<Vec<u8>>> {
        self.pages
            .get_mut(&page)
            .ok_or_else(|| io::Error::other(format!("heap page {page} is not loaded")))
    }

    fn slot(&self, heap_id: HeapId) -> io::Result<(usize, usize)> {
        let page = usize::from(heap_id.block_index());
        let index = usize::from(heap_id.index()?) + 1;
        if page >= self.count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "heap allocation is missing",
            ));
        }
        let allocations = self
            .pages
            .get(&page)
            .ok_or_else(|| io::Error::other(format!("heap page {page} is not loaded")))?;
        if index >= allocations.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "heap allocation is missing",
            ));
        }
        Ok((page, index))
    }

    fn get(&self, heap_id: HeapId) -> io::Result<&[u8]> {
        let (page, index) = self.slot(heap_id)?;
        Ok(&self.pages[&page][index])
    }

    /// Overwrite an allocation in place, keeping its HID. Callers keep the size unchanged.
    fn replace(&mut self, heap_id: HeapId, data: Vec<u8>) -> io::Result<()> {
        let (page, index) = self.slot(heap_id)?;
        self.page_mut(page)?[index] = data;
        self.dirty.insert(page);
        Ok(())
    }

    /// Free an allocation: it is encoded as a zero-length page map entry so other HIDs stay valid.
    fn free(&mut self, heap_id: HeapId) -> io::Result<Vec<u8>> {
        let (page, index) = self.slot(heap_id)?;
        self.dirty.insert(page);
        Ok(std::mem::take(&mut self.page_mut(page)?[index]))
    }

    fn header_size(page: usize) -> usize {
        if page == 0 {
            12
        } else if page % 128 == 8 {
            66
        } else {
            2
        }
    }

    fn alloc(&mut self, data: Vec<u8>) -> io::Result<HeapId> {
        if data.len() > MAX_HN_ALLOCATION_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "heap allocation exceeds one Unicode PST data block",
            ));
        }

        let mut page = self.count - 1;
        let fits = |page: usize, allocations: &[Vec<u8>]| {
            let allocation_bytes = allocations.iter().map(Vec::len).sum::<usize>() + data.len();
            let allocation_count = allocations.len() + 1;
            // The trailing `+ 2` and the `< MAX` leave a few bytes spare: a page fills at most 4 bytes short. They once
            // reserved a padding entry and are kept so that pages break where they always did.
            Self::header_size(page) + allocation_bytes + 4 + 2 * (allocation_count + 1) + 2
                < MAX_DATA_BLOCK_SIZE
        };
        if !fits(page, self.page_mut(page)?) {
            // The old last page becomes non-final and must be re-encoded as a full block (zero bytes after its page
            // map, no padding entry).
            self.dirty.insert(page);
            page = self.count;
            self.pages.insert(page, vec![Vec::new()]);
            self.dirty.insert(page);
            self.count += 1;
            if !fits(page, &self.pages[&page]) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "heap allocation does not fit in a Unicode PST data block",
                ));
            }
        }

        let allocations = self.page_mut(page)?;
        let heap_id = HeapId::new(
            u16::try_from(allocations.len()).map_err(|_| PstError::IntegerConversion)?,
            u16::try_from(page).map_err(|_| PstError::IntegerConversion)?,
        )?;
        allocations.push(data);
        self.dirty.insert(page);
        Ok(heap_id)
    }

    /// Encode the dirty pages, in page order, keeping the builder as it is (the pages a contents-table append
    /// changed). Every dirty page must be loaded.
    /// Every encoded page's fill level is its own; a page not encoded keeps the level stored in its header page.
    fn page_level(
        &self,
        page: usize,
        encoded: &BTreeMap<usize, Vec<u8>>,
    ) -> io::Result<HeapFillLevel> {
        match encoded.get(&page) {
            Some(data) => Ok(fill_level(heap_page_free(data))),
            None => self.stored_levels.get(&page).copied().ok_or_else(|| {
                io::Error::other(format!("the fill level of heap page {page} is not known"))
            }),
        }
    }

    /// Encode the dirty pages, in page order, keeping the builder as it is (the pages a contents-table append changed),
    /// and the header pages that store their fill levels. Every one of them must be loaded.
    fn encode_dirty(
        &self,
        client_signature: HeapNodeType,
        user_root: HeapId,
    ) -> io::Result<Vec<(usize, Vec<u8>)>> {
        let mut encoded = BTreeMap::new();
        for page in self.dirty.iter().copied().chain(self.level_pages()) {
            if encoded.contains_key(&page) {
                continue;
            }
            let allocations = self
                .pages
                .get(&page)
                .ok_or_else(|| io::Error::other(format!("heap page {page} is not loaded")))?
                .clone();
            encoded.insert(
                page,
                encode_heap_page(page, self.count, allocations, client_signature, user_root)?,
            );
        }
        self.write_levels(&mut encoded)?;
        Ok(encoded.into_iter().collect())
    }

    fn write_levels(&self, encoded: &mut BTreeMap<usize, Vec<u8>>) -> io::Result<()> {
        let header_pages: Vec<usize> = encoded
            .keys()
            .copied()
            .filter(|&page| page == 0 || page % 128 == 8)
            .collect();
        for page in header_pages {
            let mut data = encoded.remove(&page).expect("listed above");
            let own = fill_level(heap_page_free(&data));
            let result = write_fill_levels(page, &mut data, self.count, |target| {
                if target == page {
                    Ok(own)
                } else {
                    self.page_level(target, encoded)
                }
            });
            encoded.insert(page, data);
            result?;
        }
        Ok(())
    }

    fn finish(self, client_signature: HeapNodeType, user_root: HeapId) -> io::Result<Vec<Vec<u8>>> {
        let count = self.count;
        if self.pages.len() != count {
            return Err(io::Error::other("heap page was not encoded"));
        }
        let mut encoded = self
            .pages
            .iter()
            .map(|(&page, allocations)| {
                Ok((
                    page,
                    encode_heap_page(
                        page,
                        count,
                        allocations.clone(),
                        client_signature,
                        user_root,
                    )?,
                ))
            })
            .collect::<io::Result<BTreeMap<_, _>>>()?;
        self.write_levels(&mut encoded)?;
        Ok(encoded.into_values().collect())
    }
}

/// Encode heap page `page` of a heap of `page_count` pages from its allocations (index 0 is the header slot). A
/// non-final page is a full data block: its page map follows the last allocation (MS-PST 2.3.1.5) and the rest of the
/// block is zero bytes, as SCANPST leaves a page it repairs (zero-filled padding allocations there make SCANPST report
/// "Items needing repair" for a message whose attachment table spills into a second page, and its repair removes them).
fn encode_heap_page(
    page: usize,
    page_count: usize,
    mut allocations: Vec<Vec<u8>>,
    client_signature: HeapNodeType,
    user_root: HeapId,
) -> io::Result<Vec<u8>> {
    let header_size = HeapBuilder::header_size(page);
    let full_block = page + 1 < page_count;
    let page_map_offset =
        (header_size + allocations.iter().skip(1).map(Vec::len).sum::<usize>()).next_multiple_of(2);
    let page_map_offset =
        u16::try_from(page_map_offset).map_err(|_| PstError::IntegerConversion)?;
    let mut header = Vec::with_capacity(header_size);
    if page == 0 {
        HeapNodeHeader::new(
            page_map_offset,
            client_signature,
            user_root,
            [HeapFillLevel::Empty; 8],
        )
        .write(&mut header)?;
    } else if page % 128 == 8 {
        HeapNodeBitmapHeader::new(page_map_offset, [HeapFillLevel::Empty; 128])
            .write(&mut header)?;
    } else {
        HeapNodePageHeader::new(page_map_offset).write(&mut header)?;
    }
    allocations[0] = header;

    let mut data = allocations.remove(0);
    let mut offsets = Vec::with_capacity(allocations.len() + 1);
    offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    for allocation in allocations {
        data.extend_from_slice(&allocation);
        offsets.push(u16::try_from(data.len()).map_err(|_| PstError::IntegerConversion)?);
    }
    data.resize(usize::from(page_map_offset), 0);
    data.write_u16::<byteorder::LittleEndian>(
        u16::try_from(offsets.len() - 1).map_err(|_| PstError::IntegerConversion)?,
    )?;
    let free_count = offsets
        .windows(2)
        .filter(|offsets| offsets[0] == offsets[1])
        .count();
    data.write_u16::<byteorder::LittleEndian>(
        u16::try_from(free_count).map_err(|_| PstError::IntegerConversion)?,
    )?;
    for offset in offsets {
        data.write_u16::<byteorder::LittleEndian>(offset)?;
    }
    if data.len() > MAX_DATA_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "encoded heap page exceeds one Unicode PST data block",
        ));
    }
    if full_block {
        data.resize(MAX_DATA_BLOCK_SIZE, 0);
    }
    Ok(data)
}

/// The bytes an encoded heap page leaves free after its page map (MS-PST 2.3.1.2's fill level counts these): a full
/// non-final block keeps them as zero bytes, a final block does not store them.
fn heap_page_free(data: &[u8]) -> usize {
    let at = |offset: usize| {
        data.get(offset..offset + 2)
            .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let page_map = at(0);
    MAX_DATA_BLOCK_SIZE.saturating_sub(page_map + 4 + 2 * (at(page_map) + 1))
}

/// The allocations of one encoded heap page as `HeapNodeInner::writer_pages` decodes them (index 0 is the header slot).
fn decode_heap_page(index: usize, data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let mut cursor = Cursor::new(data);
    let page_map_offset = match index {
        0 => HeapNodeHeader::read(&mut cursor)?.page_map_offset(),
        bitmap if bitmap % 128 == 8 => HeapNodeBitmapHeader::read(&mut cursor)?.page_map_offset(),
        _ => HeapNodePageHeader::read(&mut cursor)?.page_map_offset(),
    };
    cursor.seek(SeekFrom::Start(u64::from(page_map_offset)))?;
    let page_map = HeapNodePageMap::read(&mut cursor)?;
    let mut allocations = vec![Vec::new()];
    for allocation in page_map.allocations() {
        let start = usize::from(allocation.offset());
        let end = start + usize::from(allocation.size());
        allocations.push(
            data.get(start..end)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "heap allocation outside its page",
                    )
                })?
                .to_vec(),
        );
    }
    Ok(allocations)
}

fn property_value_bytes(value: &PropertyValue) -> io::Result<Vec<u8>> {
    // Strings are stored without a terminator, as Outlook stores them.
    let mut data = Vec::new();
    value.write(&mut data)?;
    Ok(data)
}

fn small_property_record(value: &PropertyValue) -> Option<u32> {
    match value {
        PropertyValue::Integer16(value) => Some(u32::from(*value as u16)),
        PropertyValue::Integer32(value) | PropertyValue::ErrorCode(value) => Some(*value as u32),
        PropertyValue::Floating32(value) => Some(value.to_bits()),
        PropertyValue::Boolean(value) => Some(u32::from(*value)),
        _ => None,
    }
}

type RebuiltPropertyContext = (Vec<Vec<u8>>, Vec<(NodeId, Vec<u8>)>);

/// The property context's BTH (MS-PST 2.3.3), its header written into `header`, the slot reserved first.
fn build_property_tree(
    heap: &mut HeapBuilder,
    records: &[(u16, PropertyType, PropertyValueRecord)],
    header: HeapId,
) -> io::Result<HeapId> {
    let mut children = records
        .chunks(MAX_HN_ALLOCATION_SIZE / 8)
        .map(|chunk| {
            let mut data = Vec::with_capacity(chunk.len() * 8);
            for (id, prop_type, value) in chunk {
                PropertyTreeRecord::new(*id, *prop_type, *value).write(&mut data)?;
            }
            Ok((chunk[0].0, heap.alloc(data)?))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut levels = 0;
    while children.len() > 1 {
        children = children
            .chunks(MAX_HN_ALLOCATION_SIZE / 6)
            .map(|chunk| {
                let mut data = Vec::with_capacity(chunk.len() * 6);
                for (key, child) in chunk {
                    HeapTreeIntermediateEntry::new(*key, *child).write(&mut data)?;
                }
                Ok((chunk[0].0, heap.alloc(data)?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        levels += 1;
    }
    let root = children.first().map(|entry| entry.1).unwrap_or_default();
    let mut bytes = Vec::new();
    HeapTreeHeader::new(2, 6, levels, root)?.write(&mut bytes)?;
    heap.replace(header, bytes)?;
    Ok(header)
}

/// The slot of a BTH header (8 bytes), reserved before anything else so that it is HID 0x20 in a new heap, as Outlook
/// writes every property context and every table's row index (MS-PST allows any hidUserRoot; the
/// java-libpst/pst-extractor readers read the header at 0x20 only).
fn reserve_bth_header(heap: &mut HeapBuilder) -> io::Result<HeapId> {
    heap.alloc(vec![0; 8])
}

/// The slot of a TCINFO with `context`'s columns (its size depends on the columns only), reserved right after the row
/// index's header so that it is HID 0x40 in a new heap, as Outlook writes every table.
fn reserve_tcinfo(heap: &mut HeapBuilder, context: &TableContextInfo) -> io::Result<HeapId> {
    let mut bytes = Vec::new();
    context.write(&mut bytes)?;
    heap.alloc(vec![0; bytes.len()])
}

fn property_context_pages(
    properties: impl IntoIterator<Item = (u16, PropertyValue)>,
    next_subnode_index: u32,
) -> io::Result<RebuiltPropertyContext> {
    property_context_pages_with_nodes(properties, &[], next_subnode_index)
}

/// A property context as `property_context_pages` builds it, plus records whose value is a subnode the caller writes
/// (an attachment's PidTagAttachDataBinary).
fn property_context_pages_with_nodes(
    properties: impl IntoIterator<Item = (u16, PropertyValue)>,
    nodes: &[(u16, PropertyType, NodeId)],
    next_subnode_index: u32,
) -> io::Result<RebuiltPropertyContext> {
    property_context_pages_keyed(properties, nodes, next_subnode_index).map(|(pc, _)| pc)
}

/// `property_context_pages_with_nodes`, plus the property ID of each value written in a subnode (a table cell with the
/// same value shares that subnode's block).
fn property_context_pages_keyed(
    properties: impl IntoIterator<Item = (u16, PropertyValue)>,
    nodes: &[(u16, PropertyType, NodeId)],
    mut next_subnode_index: u32,
) -> io::Result<(RebuiltPropertyContext, Vec<(u16, NodeId)>)> {
    let mut heap = HeapBuilder::new();
    let header = reserve_bth_header(&mut heap)?;
    let mut external = Vec::new();
    let mut keys = Vec::new();
    let mut records = properties
        .into_iter()
        .filter(|(_, value)| !matches!(value, PropertyValue::Null))
        .map(|(id, value)| {
            let prop_type = PropertyType::from(&value);
            let record = if let Some(small) = small_property_record(&value) {
                PropertyValueRecord::Small(small)
            } else {
                let data = property_value_bytes(&value)?;
                if data.is_empty() {
                    // An empty value is HNID 0, the encoding found in Outlook-written stores.
                    PropertyValueRecord::Heap(HeapId::default())
                } else if data.len() > MAX_HN_ALLOCATION_SIZE {
                    let node = NodeId::new(LTP_SUBNODE, next_subnode_index)?;
                    next_subnode_index = next_subnode_index
                        .checked_add(1)
                        .ok_or(PstError::IntegerConversion)?;
                    external.push((node, data));
                    keys.push((id, node));
                    PropertyValueRecord::Node(node)
                } else {
                    PropertyValueRecord::Heap(heap.alloc(data)?)
                }
            };
            Ok((id, prop_type, record))
        })
        .collect::<io::Result<Vec<_>>>()?;
    records.extend(
        nodes
            .iter()
            .map(|&(id, prop_type, node)| (id, prop_type, PropertyValueRecord::Node(node))),
    );
    records.sort_by_key(|record| record.0);
    let user_root = build_property_tree(&mut heap, &records, header)?;
    Ok((
        (heap.finish(HeapNodeType::Properties, user_root)?, external),
        keys,
    ))
}

/// Values a table cell may share, as (property ID, the value's block, its length, its digest). Outlook writes a table
/// cell too large for the heap (MS-PST 2.3.4.4) as a subnode whose block is the block of the same value in the row's
/// object (its message for a contents row, its attachment for an attachment row), with that block's reference count one
/// higher; SCANPST reports a separate copy as "row doesn't match sub-object", and its repair re-points the cell to the
/// object's block.
type SharedValues = Vec<(u16, UnicodeBlockId, usize, u64)>;

fn value_digest(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// The object's block for a cell value of `prop_id`: only when the object stores that property with the same bytes.
fn shared_block(
    shared: &[(u16, UnicodeBlockId, usize, u64)],
    prop_id: u16,
    data: &[u8],
) -> Option<UnicodeBlockId> {
    shared
        .iter()
        .find(|(id, _, len, digest)| {
            *id == prop_id && *len == data.len() && *digest == value_digest(data)
        })
        .map(|(_, block, _, _)| *block)
}

enum TableCell {
    Value(PropertyValue),
    Bytes(Vec<u8>),
    Node(NodeId),
}

type RebuiltTable = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<NodeId>);

/// A table cell whose value does not fit a heap allocation (MS-PST 2.3.3) is moved to a subnode of NID_TYPE_LTP, which
/// an HNID may name (MS-PST 2.3.4.4); Outlook follows a table cell only into such a subnode. Every table's cells go
/// this way, not only the contents rows' own byte cells, so a long given value (or a recipient name of more than 1,790
/// characters) does not fail the append.
fn spill_table_cell(
    cell: &mut TableCell,
    prop_type: PropertyType,
    next_subnode_index: &mut u32,
    external: &mut Vec<(NodeId, Vec<u8>)>,
) -> io::Result<()> {
    if !table_pointer_type(prop_type) {
        return Ok(());
    }
    let data = match cell {
        TableCell::Bytes(data) if data.len() > MAX_HN_ALLOCATION_SIZE => std::mem::take(data),
        TableCell::Value(value) => {
            let data = property_value_bytes(value)?;
            if data.len() <= MAX_HN_ALLOCATION_SIZE {
                return Ok(());
            }
            data
        }
        _ => return Ok(()),
    };
    let node = NodeId::new(LTP_SUBNODE, *next_subnode_index)?;
    *next_subnode_index = next_subnode_index
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?;
    external.push((node, data));
    *cell = TableCell::Node(node);
    Ok(())
}

fn table_pointer_type(prop_type: PropertyType) -> bool {
    matches!(
        prop_type,
        PropertyType::String8
            | PropertyType::Unicode
            | PropertyType::Guid
            | PropertyType::Binary
            | PropertyType::Object
            | PropertyType::MultipleInteger16
            | PropertyType::MultipleInteger32
            | PropertyType::MultipleFloating32
            | PropertyType::MultipleFloating64
            | PropertyType::MultipleCurrency
            | PropertyType::MultipleFloatingTime
            | PropertyType::MultipleInteger64
            | PropertyType::MultipleString8
            | PropertyType::MultipleUnicode
            | PropertyType::MultipleTime
            | PropertyType::MultipleGuid
            | PropertyType::MultipleBinary
    )
}

fn write_fixed_table_value(
    mut target: &mut [u8],
    prop_type: PropertyType,
    value: &PropertyValue,
) -> io::Result<()> {
    match (prop_type, value) {
        (PropertyType::Integer16, PropertyValue::Integer16(value)) => {
            target.write_i16::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Integer32, PropertyValue::Integer32(value)) => {
            target.write_i32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Floating32, PropertyValue::Floating32(value)) => {
            target.write_f32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Floating64, PropertyValue::Floating64(value)) => {
            target.write_f64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Currency, PropertyValue::Currency(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::FloatingTime, PropertyValue::FloatingTime(value)) => {
            target.write_f64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::ErrorCode, PropertyValue::ErrorCode(value)) => {
            target.write_i32::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Boolean, PropertyValue::Boolean(value)) => target.write_u8(u8::from(*value)),
        (PropertyType::Integer64, PropertyValue::Integer64(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        (PropertyType::Time, PropertyValue::Time(value)) => {
            target.write_i64::<byteorder::LittleEndian>(*value)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("table value does not match {prop_type:?}"),
        )),
    }
}

fn encode_table_row(
    context: &TableContextInfo,
    heap: &mut HeapBuilder,
    row_id: impl Into<u32>,
    unique: u32,
    cells: Vec<Option<TableCell>>,
) -> io::Result<Vec<u8>> {
    let mut row = vec![0; usize::from(context.end_existence_bitmap())];
    (&mut row[0..4]).write_u32::<byteorder::LittleEndian>(row_id.into())?;
    (&mut row[4..8]).write_u32::<byteorder::LittleEndian>(unique)?;

    for (column, cell) in context.columns().iter().zip(cells) {
        let Some(cell) = cell else { continue };
        let bit = usize::from(column.existence_bitmap_index());
        row[usize::from(context.end_1byte_values()) + bit / 8] |= 0x80 >> (bit % 8);

        let offset = usize::from(column.offset());
        let size = usize::from(column.size());
        let mut target = &mut row[offset..offset + size];
        if table_pointer_type(column.prop_type()) {
            let data = match cell {
                TableCell::Node(node) => Err(u32::from(node)),
                TableCell::Bytes(data) => Ok(data),
                TableCell::Value(value) => Ok(property_value_bytes(&value)?),
            };
            let value = match data {
                Err(node) => node,
                // A present, empty value is HNID 0 with its CEB bit set, as in Outlook-written tables (the template's
                // 0x001A cells). A zero-size allocation is read by an independent reader as a freed item and hides the
                // whole folder.
                Ok(data) if data.is_empty() => 0,
                Ok(data) => u32::from(heap.alloc(data)?),
            };
            target.write_u32::<byteorder::LittleEndian>(value)?;
        } else {
            let TableCell::Value(value) = cell else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixed-width table column has an indirect value",
                ));
            };
            write_fixed_table_value(target, column.prop_type(), &value)?;
        }
    }
    Ok(row)
}

/// A table's row index (MS-PST 2.3.4.3), its BTH header written into `header`, a slot reserved before the rows or the
/// old header's slot when a loaded table is rebuilt.
fn build_row_index<T>(heap: &mut HeapBuilder, rows: &[T], header: HeapId) -> io::Result<HeapId>
where
    T: Copy,
    u32: From<T>,
{
    let children = rows
        .chunks(ROW_INDEX_FANOUT)
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let mut data = Vec::with_capacity(chunk.len() * 8);
            for (index, row) in chunk.iter().enumerate() {
                TableRowId::new(u32::from(*row)).write(&mut data)?;
                data.write_u32::<byteorder::LittleEndian>(
                    u32::try_from(chunk_index * ROW_INDEX_FANOUT + index)
                        .map_err(|_| PstError::IntegerConversion)?,
                )?;
            }
            Ok((u32::from(chunk[0]), heap.alloc(data)?))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let bytes = row_index_header(heap, children, 0)?;
    heap.replace(header, bytes)?;
    Ok(header)
}

const ROW_INDEX_FANOUT: usize = MAX_HN_ALLOCATION_SIZE / 8;

/// Stack intermediate levels over `children` (`(first row id, node)`) and encode the BTH header.
fn row_index_header(
    heap: &mut HeapBuilder,
    mut children: Vec<(u32, HeapId)>,
    mut levels: u8,
) -> io::Result<Vec<u8>> {
    while children.len() > 1 {
        children = children
            .chunks(ROW_INDEX_FANOUT)
            .map(|chunk| {
                let mut data = Vec::with_capacity(chunk.len() * 8);
                for (key, child) in chunk {
                    HeapTreeIntermediateEntry::new(TableRowId::new(*key), *child)
                        .write(&mut data)?;
                }
                Ok((chunk[0].0, heap.alloc(data)?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        levels += 1;
    }
    let root = children.first().map(|entry| entry.1).unwrap_or_default();
    let mut header = Vec::new();
    HeapTreeHeader::new(4, 4, levels, root)?.write(&mut header)?;
    Ok(header)
}

/// Free the right spine of a row-index BTH, root first, returning each node without its last
/// (rewritten) child entry. Freeing before new rows are encoded lets them reuse the space.
/// Free every node of a row-index BTH from `heap`; its header stays allocated (a rebuild writes the new header into the
/// same slot). A zero header HID is an absent index.
fn free_row_index_nodes(heap: &mut HeapBuilder, header: HeapId) -> io::Result<()> {
    if u32::from(header) == 0 {
        return Ok(());
    }
    let data = heap.get(header)?.to_vec();
    let index = HeapTreeHeader::read(&mut data.as_slice())?;
    let record = usize::from(index.key_size()) + 4;
    let mut nodes = if u32::from(index.root()) == 0 {
        Vec::new()
    } else {
        vec![index.root()]
    };
    for level in (0..=index.levels()).rev() {
        let mut children = Vec::new();
        for node in nodes {
            let data = heap.free(node)?;
            if level > 0 {
                if data.is_empty() || data.len() % record != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "table row index node is malformed",
                    ));
                }
                for entry in data.chunks(record) {
                    children.push(HeapId::from(u32::from_le_bytes(
                        entry[record - 4..].try_into().unwrap(),
                    )));
                }
            }
        }
        nodes = children;
    }
    Ok(())
}

fn take_row_index_spine(
    heap: &mut HeapBuilder,
    index: &HeapTreeHeader,
) -> io::Result<Vec<Vec<u8>>> {
    let mut node = index.root();
    let mut spine = Vec::new();
    for level in (0..=index.levels()).rev() {
        let mut data = heap.free(node)?;
        if data.is_empty() || data.len() % 8 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "table row index node is malformed",
            ));
        }
        if level > 0 {
            let last = data.len() - 8;
            node = HeapId::from(u32::from_le_bytes(data[last + 4..].try_into().unwrap()));
            data.truncate(last);
        }
        spine.push(data);
    }
    Ok(spine)
}

/// Re-allocate a spine from `take_row_index_spine` with `(row id, row index)` records appended
/// and return the new BTH header.
fn append_row_index_spine(
    heap: &mut HeapBuilder,
    mut spine: Vec<Vec<u8>>,
    levels: u8,
    records: &[(u32, u32)],
) -> io::Result<Vec<u8>> {
    let mut children: Vec<(u32, HeapId)> = Vec::new();
    let mut leaf = true;
    while let Some(mut data) = spine.pop() {
        if leaf {
            for (key, value) in records {
                data.extend_from_slice(&key.to_le_bytes());
                data.extend_from_slice(&value.to_le_bytes());
            }
            leaf = false;
        }
        for (key, child) in children {
            data.extend_from_slice(&key.to_le_bytes());
            data.extend_from_slice(&u32::from(child).to_le_bytes());
        }
        children = data
            .chunks(ROW_INDEX_FANOUT * 8)
            .map(|chunk| {
                let key = u32::from_le_bytes(chunk[..4].try_into().unwrap());
                Ok((key, heap.alloc(chunk.to_vec())?))
            })
            .collect::<io::Result<Vec<_>>>()?;
    }
    row_index_header(heap, children, levels)
}

/// Byte patches of one row: (offset, bytes, CEB bit).
type RowPatches = Vec<(usize, Vec<u8>, Option<usize>)>;
/// A table after a row patch: its heap root, and its row-matrix subnode with the new block when the rows are in one.
type PatchedTable = (UnicodeBlockId, Option<(NodeId, UnicodeBlockId)>);

/// The patches that overwrite fixed-width `cells` of a row and bump its row version. `None` when a cell has no
/// fixed-width column of its type.
fn row_patches(
    context: &TableContextInfo,
    cells: &[(u16, PropertyValue)],
    row_version: u32,
) -> io::Result<Option<RowPatches>> {
    let version = PropertyValue::Integer32(row_version as i32);
    let mut patches = vec![(4, row_version.to_le_bytes().to_vec(), None)];
    for (prop_id, value) in cells.iter().chain([&(LTP_ROW_VERSION_PROP_ID, version)]) {
        let column = context.columns().iter().find(|column| {
            column.prop_id() == *prop_id && column.prop_type() == PropertyType::from(value)
        });
        let Some(column) = column.filter(|column| !table_pointer_type(column.prop_type())) else {
            if *prop_id == LTP_ROW_VERSION_PROP_ID {
                continue;
            }
            return Ok(None);
        };
        let mut bytes = vec![0; usize::from(column.size())];
        write_fixed_table_value(&mut bytes, column.prop_type(), value)?;
        patches.push((
            usize::from(column.offset()),
            bytes,
            Some(usize::from(column.existence_bitmap_index())),
        ));
    }
    Ok(Some(patches))
}

/// Apply `row_patches` to one row's bytes; the row must be the one whose row ID is `row` (its first four bytes).
fn apply_row_patches(
    context: &TableContextInfo,
    row_data: &mut [u8],
    row: NodeId,
    patches: &RowPatches,
) -> io::Result<()> {
    if row_data.len() < 8 || u32::from_le_bytes(row_data[..4].try_into().unwrap()) != u32::from(row)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the table's row index points at another row",
        ));
    }
    for (offset, bytes, bit) in patches {
        row_data[*offset..offset + bytes.len()].copy_from_slice(bytes);
        if let Some(bit) = bit {
            row_data[usize::from(context.end_1byte_values()) + bit / 8] |= 0x80 >> (bit % 8);
        }
    }
    Ok(())
}

/// Overwrite fixed-width cells of one row found through the row index, bumping its row version. Rows in a subnode: only
/// the row's block and the index blocks over it are written, and the heap is kept. Rows in the heap: the row-matrix
/// allocation is replaced at the same size, and only its page and the heap's index blocks are written. Returns the
/// table's new heap root and the row-matrix subnode with its new block (`None` for rows in the heap), or `None` when
/// the row is not in the index, a cell has no fixed-width column or the layout is not one this takes; callers then
/// rebuild the table.
#[allow(clippy::too_many_arguments)]
fn patch_table_row_by_index(
    file: &mut File,
    tx: &mut Tx<'_>,
    heap_root: UnicodeBlockId,
    mut view: TableView,
    row: NodeId,
    cells: &[(u16, PropertyValue)],
    row_version: u32,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
) -> io::Result<Option<PatchedTable>> {
    let row_size = usize::from(view.context.end_existence_bitmap());
    if !(8..=MAX_DATA_BLOCK_SIZE).contains(&row_size) {
        return Ok(None);
    }
    let Some(patches) = row_patches(&view.context, cells, row_version)? else {
        return Ok(None);
    };
    let Some(position) = find_table_row(file, tx, &mut view, u32::from(row))? else {
        return Ok(None);
    };
    if let Some(rows_tree) = view.rows_tree.as_mut() {
        let rows_per_block = MAX_DATA_BLOCK_SIZE / row_size;
        let leaf_index = position / rows_per_block;
        if leaf_index >= rows_tree.leaves {
            return Ok(None);
        }
        let leaf = rows_tree.leaf(file, tx, leaf_index)?;
        let mut data = read_data_block(file, tx, leaf)?;
        let start = position % rows_per_block * row_size;
        let Some(row_data) = data.get_mut(start..start + row_size) else {
            return Ok(None);
        };
        apply_row_patches(&view.context, row_data, row, &patches)?;
        let rows = edit_data_tree(
            file,
            tx,
            blocks,
            next_bid,
            next_offset,
            rows_tree,
            BTreeMap::from([(leaf_index, data)]),
        )?;
        let rows_node = view
            .context
            .rows()
            .ok_or_else(|| io::Error::other("table rows node is missing"))?;
        return Ok(Some((heap_root, Some((rows_node, rows)))));
    }
    let Some(rows) = view
        .context
        .rows()
        .filter(|rows| matches!(rows.id_type(), Ok(NodeIdType::HeapNode)))
    else {
        return Ok(None);
    };
    let rows = HeapId::from(u32::from(rows));
    ensure_heap_page(
        file,
        tx,
        &mut view.heap_tree,
        &mut view.heap,
        usize::from(rows.block_index()),
    )?;
    let mut data = view.heap.get(rows)?.to_vec();
    let start = position * row_size;
    let Some(row_data) = data.get_mut(start..start + row_size) else {
        return Ok(None);
    };
    apply_row_patches(&view.context, row_data, row, &patches)?;
    view.heap.replace(rows, data)?;
    ensure_level_pages(file, tx, &mut view.heap_tree, &mut view.heap)?;
    let pages = view
        .heap
        .encode_dirty(HeapNodeType::Table, view.user_root)?;
    let heap = edit_data_tree(
        file,
        tx,
        blocks,
        next_bid,
        next_offset,
        &mut view.heap_tree,
        pages.into_iter().collect(),
    )?;
    Ok(Some((heap, None)))
}

fn string_table_cell(prop_type: PropertyType, value: &str) -> Option<TableCell> {
    match prop_type {
        PropertyType::String8 => Some(TableCell::Bytes(value.as_bytes().to_vec())),
        PropertyType::Unicode => Some(TableCell::Bytes(utf16_bytes(value))),
        _ => None,
    }
}

/// The display name of an address: its name, or the address when the name is empty, as MS-OXCMAIL 2.2.3.2's MIME reader
/// stores it ("If there is not a matching address book entry and there is no display name part, use the Internet e-mail
/// address part"), for recipients, the sender and the sent-representing identity alike.
fn display_name<'a>(name: &'a str, email: &'a str) -> &'a str {
    if name.is_empty() {
        email
    } else {
        name
    }
}

fn recipient_display(
    recipients: &[UnicodePstRecipient<'_>],
    recipient_type: UnicodePstRecipientType,
) -> String {
    recipients
        .iter()
        .filter(|recipient| recipient.recipient_type == recipient_type)
        .map(|recipient| display_name(recipient.name, recipient.email))
        .collect::<Vec<_>>()
        .join("; ")
}

fn recipient_display_table_cell(
    prop_type: PropertyType,
    recipients: &[UnicodePstRecipient<'_>],
    recipient_type: UnicodePstRecipientType,
) -> Option<TableCell> {
    let display = recipient_display(recipients, recipient_type);
    (!display.is_empty())
        .then(|| string_table_cell(prop_type, &display))
        .flatten()
}

/// A message's recipient table, attachment table and attachments' property contexts. `size` is their share of
/// PidTagMessageSize: every byte they store, a row matrix in several blocks with each block but the last padded to
/// 8,176 bytes (MS-PST 2.3.4.4), plus the attachments' content. They are built before the commit writes anything, for
/// their input errors and the size refusal, and again while the message is written. Holding them for the whole batch
/// instead raises the peak RSS by about 18 MB on a 10,000-message manifest (default batch 4,096); building them twice
/// costs about 7 % of the CPU time there.
struct MessageParts {
    recipients: RebuiltRecipientTable,
    attachment_table: RebuiltRecipientTable,
    /// Each attachment-table cell in a subnode: (its node, the attachment's position, the column's property ID).
    attachment_cells: Vec<(NodeId, usize, u16)>,
    attachments: Vec<BuiltAttachment>,
    size: usize,
}

/// One attachment as it is written: its node, its property context, and its content's subnode: the data of a file (none
/// for empty content), or an attached message, built with its own parts.
struct BuiltAttachment {
    node: NodeId,
    content_node: Option<NodeId>,
    /// The property context's heap pages and its values in subnodes.
    pc: RebuiltPropertyContext,
    /// The property ID of each value in `pc`'s subnodes.
    pc_ids: Vec<(u16, NodeId)>,
    message: Option<Box<BuiltMessage>>,
}

/// A message as it is written: its property context's pages with PidTagMessageSize set, its external values and its
/// parts (the top-level message and every attached message are built the same way).
struct BuiltMessage {
    pages: Vec<Vec<u8>>,
    external: Vec<(NodeId, Vec<u8>)>,
    /// The property ID of each value in `external`.
    external_ids: Vec<(u16, NodeId)>,
    parts: MessageParts,
    size: i32,
}

impl BuiltMessage {
    /// Every byte the message's node and subnodes hold: its PidTagMessageSize.
    fn stored_len(&self) -> usize {
        self.size as usize
    }
}

/// Attached messages may nest this deep below the top-level message: a bound on the writer's recursion for any caller.
/// The converter's own limit, from the manifest, is 8.
const MAX_ATTACHED_MESSAGE_DEPTH: usize = 32;

fn build_message(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    names: &NameIds,
) -> io::Result<BuiltMessage> {
    build_message_at(input, attachments, row_version, names, 0)
}

fn build_message_at(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    names: &NameIds,
    depth: usize,
) -> io::Result<BuiltMessage> {
    let parts = message_parts_at(input, attachments, row_version, names, depth)?;
    // Built once, with PidTagMessageSize set once its size is known (an inline Integer32: no length changes).
    let ((mut pages, external), external_ids) = message_data_keyed(input, attachments, names)?;
    let size = message_size(parts.size, &pages, &external)?;
    set_inline_property(&mut pages, 0x0E08, 0x0003, size as u32)?;
    Ok(BuiltMessage {
        pages,
        external,
        external_ids,
        parts,
        size,
    })
}

fn message_parts(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    names: &NameIds,
) -> io::Result<MessageParts> {
    message_parts_at(input, attachments, row_version, names, 0)
}

fn message_parts_at(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    names: &NameIds,
    depth: usize,
) -> io::Result<MessageParts> {
    // The input rules first, so that the recipient table is built only from valid given properties.
    check_message(input)?;
    let recipients = recipient_table_data(input.recipients, row_version)?;
    let attachment_ids = (0..attachments.len())
        .map(|index| {
            let index = u32::try_from(index).map_err(|_| PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(NodeIdType::Attachment, 0x100 + index)?)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut size = 0_usize;
    let mut built = Vec::with_capacity(attachments.len());
    let mut sizes = Vec::with_capacity(attachments.len());
    for (index, (id, attachment)) in attachment_ids.iter().copied().zip(attachments).enumerate() {
        let index = u32::try_from(index).map_err(|_| PstError::IntegerConversion)?;
        let (content_node, pc, content_len, message, attach_size) =
            if let Some(attached) = attachment.attached_message.as_deref() {
                check_attached_message(attachment)?;
                if depth >= MAX_ATTACHED_MESSAGE_DEPTH {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                        "attached messages nest deeper than {MAX_ATTACHED_MESSAGE_DEPTH} levels"
                    ),
                    ));
                }
                // The attached message is a subnode of the attachment node, "a fully formed message" not in the NBT (MS-PST
                // 2.4.6.2.2), of NID_TYPE_NORMAL_MESSAGE, as the PSTFileFormat engine writes it.
                let message = build_message_at(
                    &attached.message,
                    &attached.attachments,
                    row_version,
                    names,
                    depth + 1,
                )?;
                let node = NodeId::new(NodeIdType::NormalMessage, 0x200 + index)?;
                // PtypObject's ulSize is the attached message's PidTagMessageSize, as in an Outlook-written sample (both
                // attached messages: ulSize 4,500 and 4,465 = their PidTagMessageSize).
                let object_size = message.size as usize;
                let attach_size = attached_message_size(attachment, object_size)?;
                let pc = attached_message_data(attachment, node, object_size, attach_size)?;
                (
                    Some(node),
                    (pc, Vec::new()),
                    message.stored_len(),
                    Some(Box::new(message)),
                    attach_size,
                )
            } else {
                let len = attachment_len(attachment)?;
                let data_node = (len != 0)
                    .then(|| NodeId::new(LTP_SUBNODE, 0x200 + index))
                    .transpose()?;
                let pc = attachment_data_keyed(attachment, data_node)?;
                (data_node, pc, len, None, attachment_size(attachment)?)
            };
        let (pc, pc_ids) = pc;
        size =
            pc.0.iter()
                .chain(pc.1.iter().map(|(_, data)| data))
                .try_fold(size, |size, data| size.checked_add(data.len()))
                .and_then(|size| size.checked_add(content_len))
                .ok_or(PstError::IntegerConversion)?;
        sizes.push(attach_size);
        built.push(BuiltAttachment {
            node: id,
            content_node,
            pc,
            pc_ids,
            message,
        });
    }
    let (attachment_table, attachment_cells) =
        attachment_table_data_keyed(&attachment_ids, attachments, &sizes, row_version)?;
    // The attachment table counts only when it is written, that is when the message has attachments
    // (`push_attachments`).
    let written_table = (!attachments.is_empty()).then_some(&attachment_table);
    // Each table's heap and row matrix are data trees, written by `push_data_tree_chunks` with every block but the last
    // padded to 8,176 bytes (MS-PST 2.3.4.4: whole rows per block, "each block except the last one MUST have a size of
    // 8192 bytes"). The padding is held by the subnode, so it counts, as the stored blocks' sizes do. Counted at its
    // rows' length, a row matrix of more than one block would be 8,176 mod the row width short per block (a 65-byte
    // recipient row: 51 bytes; an attachment row: 1).
    size = [Some(&recipients.0), written_table.map(|table| &table.0)]
        .into_iter()
        .flatten()
        .map(|pages| data_tree_len(pages))
        .chain(
            [Some(&recipients.1), written_table.map(|table| &table.1)]
                .into_iter()
                .flatten()
                .flatten()
                .flat_map(|rows| {
                    std::iter::once(data_tree_len(&rows.1))
                        .chain(rows.2.iter().map(|(_, data)| data.len()))
                }),
        )
        .try_fold(size, |size, len| size.checked_add(len))
        .ok_or(PstError::IntegerConversion)?;
    Ok(MessageParts {
        recipients,
        attachment_table,
        attachment_cells,
        attachments: built,
        size,
    })
}

/// The bytes a data tree of these chunks holds once written by `push_data_tree_chunks`: every chunk but the last padded
/// to a whole data block.
fn data_tree_len(chunks: &[Vec<u8>]) -> usize {
    chunks.split_last().map_or(0, |(last, rest)| {
        rest.len() * MAX_DATA_BLOCK_SIZE + last.len()
    })
}

/// An attached message carries no file content.
fn check_attached_message(attachment: &UnicodePstAttachment<'_>) -> io::Result<()> {
    if !attachment.data.is_empty() || attachment.stream.is_some() || attachment.content_id.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an attached message has no data, stream or Content-ID",
        ));
    }
    if attachment.filename.is_empty() {
        // An empty value would be a zero-size allocation, so a name is required.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an attached message needs a display name",
        ));
    }
    Ok(())
}

/// PidTagAttachSize of an attached message: the attachment's property values (three Integer32, the display name, the
/// Boolean; the PtypObject record is not counted) plus PtypObject's ulSize. Outlook's rule, as measured in an
/// Outlook-written sample (8,078 = 3,578 + 4,500 and 8,043 = 3,578 + 4,465), and the PSTFileFormat engine's, which its
/// maintainers report SCANPST recomputes (`AttachmentObject.cs`).
fn attached_message_size(
    attachment: &UnicodePstAttachment<'_>,
    object_size: usize,
) -> io::Result<i32> {
    let size = object_size
        .checked_add(3 * 4 + 1)
        .and_then(|size| size.checked_add(utf16_bytes(attachment.filename).len()))
        .ok_or(PstError::IntegerConversion)?;
    // An attachment object larger than PidTagAttachSize can state is refused by name.
    i32::try_from(size).map_err(|_| too_large(size))
}

/// An attached message's attachment property context: PidTagAttachMethod 5 and PidTagAttachDataObject, the PtypObject
/// structure `{Nid, ulSize}` in the heap (MS-PST 2.3.3.5), with ulSize the attached message's PidTagMessageSize (as
/// Outlook writes it). Built like a message's property context, so a display name over 3,580 bytes is a subnode (LTP
/// from 0x80; the attached message is NID_TYPE_NORMAL_MESSAGE 0x200 + its position).
fn attached_message_data(
    attachment: &UnicodePstAttachment<'_>,
    node: NodeId,
    object_size: usize,
    size: i32,
) -> io::Result<RebuiltPropertyContext> {
    let object_size = u32::try_from(object_size).map_err(|_| PstError::IntegerConversion)?;
    let properties = [
        (0x0E20, PropertyValue::Integer32(size)),
        (
            0x3001,
            PropertyValue::Unicode(UnicodeValue::new(
                attachment.filename.encode_utf16().collect(),
            )),
        ),
        (
            0x3701,
            PropertyValue::Object(ObjectValue::new(node, object_size)),
        ),
        (0x3705, PropertyValue::Integer32(5)),
        (0x370B, PropertyValue::Integer32(-1)),
        (0x7FFE, PropertyValue::Boolean(false)),
    ];
    property_context_pages(properties, 0x80)
}

/// The check before a commit writes anything: a message's parts are built, for their input errors, and only a message
/// whose upper bound passes PidTagMessageSize's limit has its property context built here too, so that it is refused as
/// too large before anything is written. Every other property context is built once, while the message is written.
fn prepare_message(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    row_version: u32,
    names: &NameIds,
) -> io::Result<()> {
    let parts = message_parts(input, attachments, row_version, names)?.size;
    if parts as u64 + message_data_bound(input) > i32::MAX as u64 + 6 {
        let (pages, external) = message_data(input, attachments, names)?;
        message_size(parts, &pages, &external)?;
    }
    Ok(())
}

/// PidTagMessageSize: the message's property context and external values plus its parts (`MessageParts::size`).
/// PidTagMessageSize is every byte the message's node and subnodes hold, as classic Outlook stores it (20 messages it
/// wrote) and SCANPST's repair sets it.
fn message_size(
    parts: usize,
    pages: &[Vec<u8>],
    external: &[(NodeId, Vec<u8>)],
) -> io::Result<i32> {
    let size = pages
        .iter()
        .chain(external.iter().map(|(_, data)| data))
        .try_fold(parts, |size, data| size.checked_add(data.len()))
        .ok_or(PstError::IntegerConversion)?;
    // A message larger than PidTagMessageSize can state is refused by name.
    i32::try_from(size).map_err(|_| too_large(size))
}

/// An upper bound on the bytes of a message's property context and external values, `message_data`. A value is at
/// most twice its UTF-8 length as UTF-16 (strings are stored without a terminator); "+ 2" covers the "; " between
/// recipient display names. The heap starts a page only when an allocation does not fit the last one, so any two
/// consecutive pages hold more than one page of allocations and the pages are at most about twice the allocations plus
/// one page; three times the values and 1 MiB cover that, the page maps and the fixed properties.
/// `a_message_size_bound_is_never_below_the_exact_size` checks it against the exact size.
fn message_data_bound(input: &UnicodePstMessage<'_>) -> u64 {
    let text = |value: &str| 2 * value.len() as u64 + 2;
    // An empty sender name is written as the address; the factor 3 below covers that (the address is then written four
    // times, the bound counts it six).
    let sender = input.sender.map_or(0, |sender| {
        text(sender.name) + text(sender.address_type) + text(sender.email)
    });
    let message_id = input.message_id.unwrap_or("");
    let values = 2 * (text(input.subject) + sender)
        + text(input.message_class)
        + text(input.body)
        + text(message_id)
        + message_id.len() as u64
        + input.search_key.map_or(0, |key| key.len() as u64)
        + input.html_body.map_or(0, |html| html.len() as u64)
        + input.transport_headers.map_or(0, text)
        + input.recipients.iter().map(|recipient| text(recipient.name) + text(recipient.email)).sum::<u64>()
        + input.categories.iter().map(|category| text(category) + 4).sum::<u64>()
        // Each given property's value, and 16 bytes for its BTH record and allocation header; the same for each named
        // one.
        + input.properties.iter().map(|property| property.value.bound() + 16).sum::<u64>()
        + input.named_properties.iter().map(|property| property.value.bound() + 16).sum::<u64>();
    3 * values + (1 << 20)
}

/// Set an inline four-byte property value in an encoded property context's heap pages: the property's record is
/// found through the BTH and its dwValueHnid overwritten (MS-PST v20250218: HID 2.3.1.1, HNHDR 2.3.1.2, HNPAGEMAP 2.3.1.5,
/// BTH 2.3.2, PC BTH record 2.3.3.3). The value is inline, so no length or allocation changes.
fn set_inline_property(
    pages: &mut [Vec<u8>],
    prop_id: u16,
    prop_type: u16,
    value: u32,
) -> io::Result<()> {
    let invalid = |detail: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("property context: {detail}"),
        )
    };
    let u16_at = |data: &[u8], at: usize| {
        data.get(at..at + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    // An allocation as (page, start, end), from its page's HNPAGEMAP.
    let allocation = |pages: &[Vec<u8>], hid: u32| -> io::Result<(usize, usize, usize)> {
        let (page, index) = ((hid >> 16) as usize, ((hid >> 5) & 0x7FF) as usize);
        let data = pages
            .get(page)
            .ok_or_else(|| invalid("an HID's page is missing"))?;
        let map = usize::from(u16_at(data, 0).ok_or_else(|| invalid("page too short"))?);
        let count =
            usize::from(u16_at(data, map).ok_or_else(|| invalid("page map outside the page"))?);
        if hid & 0x1F != 0 || index == 0 || index > count {
            return Err(invalid("an HID is not an allocation of its page"));
        }
        let offset = |at: usize| {
            u16_at(data, map + 4 + 2 * at)
                .map(usize::from)
                .ok_or_else(|| invalid("page map too short"))
        };
        let (start, end) = (offset(index - 1)?, offset(index)?);
        if start > end || end > data.len() {
            return Err(invalid("an allocation is outside its page"));
        }
        Ok((page, start, end))
    };
    let first = pages.first().ok_or_else(|| invalid("no pages"))?;
    let user_root = first
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| invalid("header too short"))?;
    let (page, start, end) = allocation(pages, user_root)?;
    let header = &pages[page][start..end];
    if header.len() != 8 || header[0] != 0xB5 || header[1] != 2 || header[2] != 6 {
        return Err(invalid("the root is not a property context BTH header"));
    }
    let levels = header[3];
    let mut node = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    for level in (0..=levels).rev() {
        let (page, start, end) = allocation(pages, node)?;
        let record = if level > 0 { 6 } else { 8 };
        let data = &pages[page][start..end];
        if data.len() % record != 0 {
            return Err(invalid("a BTH node is not a whole number of records"));
        }
        let mut keys = data
            .chunks(record)
            .map(|r| u16::from_le_bytes([r[0], r[1]]));
        if level > 0 {
            let at = keys
                .take_while(|key| *key <= prop_id)
                .count()
                .checked_sub(1)
                .ok_or_else(|| invalid("property not found"))?;
            let next = &data[at * 6 + 2..at * 6 + 6];
            node = u32::from_le_bytes([next[0], next[1], next[2], next[3]]);
        } else {
            let at = keys
                .position(|key| key == prop_id)
                .ok_or_else(|| invalid("property not found"))?;
            if u16::from_le_bytes([data[at * 8 + 2], data[at * 8 + 3]]) != prop_type {
                return Err(invalid("the property has another type"));
            }
            let value_at = start + at * 8 + 4;
            pages[page][value_at..value_at + 4].copy_from_slice(&value.to_le_bytes());
            return Ok(());
        }
    }
    Err(invalid("property not found"))
}

#[allow(clippy::too_many_arguments)]
fn new_message_table_cell(
    column: &TableColumnDescriptor,
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    store_record_key: [u8; 16],
    folder: NodeId,
    message: NodeId,
    row_version: u32,
    message_size: i32,
    names: &NameIds,
) -> io::Result<Option<TableCell>> {
    let prop_type = column.prop_type();
    let cell = match column.prop_id() {
        LTP_ROW_ID_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(u32::from(message) as i32),
        )),
        LTP_ROW_VERSION_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(row_version as i32),
        )),
        0x001A => string_table_cell(prop_type, input.message_class),
        0x0037 | 0x0E1D | 0x0070 | 0x3001 => string_table_cell(prop_type, input.subject),
        0x0042 | 0x0C1A => input.sender.and_then(|sender| {
            string_table_cell(prop_type, display_name(sender.name, sender.email))
        }),
        0x0C1F | 0x0065 => input
            .sender
            .and_then(|sender| string_table_cell(prop_type, sender.email)),
        0x0C1E | 0x0064 => input
            .sender
            .and_then(|sender| string_table_cell(prop_type, sender.address_type)),
        0x0E02 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::Bcc)
        }
        0x0E03 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::Cc)
        }
        0x0E04 => {
            recipient_display_table_cell(prop_type, input.recipients, UnicodePstRecipientType::To)
        }
        0x1035 => input
            .message_id
            .and_then(|id| string_table_cell(prop_type, id)),
        0x0039 if prop_type == PropertyType::Time => input
            .client_submit_time
            .map(|time| TableCell::Value(PropertyValue::Time(time))),
        0x0E06 if prop_type == PropertyType::Time => input
            .delivery_time
            .map(|time| TableCell::Value(PropertyValue::Time(time))),
        0x3008 if prop_type == PropertyType::Time => Some(TableCell::Value(PropertyValue::Time(
            input.last_modification_time,
        ))),
        0x0017 if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(input.importance.importance()),
        )),
        0x0E17 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(0)))
        }
        0x0E07 if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(message_flags(input, attachments)),
        )),
        0x0E08 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(message_size)))
        }
        0x0E1B if prop_type == PropertyType::Boolean => Some(TableCell::Value(
            PropertyValue::Boolean(!attachments.is_empty()),
        )),
        0x3FDE if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(65001)))
        }
        0x0E09 | 0x0FFF if prop_type == PropertyType::Binary => {
            let node = if column.prop_id() == 0x0E09 {
                folder
            } else {
                message
            };
            let entry_id = EntryId::new(StoreRecordKey::new(store_record_key), node);
            Some(TableCell::Bytes(Vec::try_from(&entry_id)?))
        }
        0x300B if prop_type == PropertyType::Binary => {
            Some(TableCell::Bytes(search_key_bytes(input)))
        }
        0x3013 if prop_type == PropertyType::Binary => None,
        // A given named property fills its column the same way, by the ID the store's map gives it.
        id if id >= 0x8000 => input
            .named_properties
            .iter()
            .find(|property| {
                names.given(property).ok() == Some(id) && property.value.prop_type() == prop_type
            })
            .map(|property| TableCell::Value(property.value.property_value())),
        // A given property fills its column when the folder's contents table has it, with the same type.
        id => input
            .properties
            .iter()
            .find(|property| property.id == id && property.value.prop_type() == prop_type)
            .map(|property| TableCell::Value(property.value.property_value())),
    };
    Ok(cell)
}

fn rebuild_table_with_rows_and_version(
    table: &dyn TableContext,
    new_rows: Vec<(NodeId, Vec<Option<TableCell>>)>,
    rows_node: Option<NodeId>,
    row_version: u32,
) -> io::Result<RebuiltTable> {
    rebuild_table(table, new_rows, rows_node, row_version, None, false)
}

fn rebuild_table(
    table: &dyn TableContext,
    new_rows: Vec<(NodeId, Vec<Option<TableCell>>)>,
    rows_node: Option<NodeId>,
    row_version: u32,
    property_update: Option<(NodeId, &[(u16, PropertyValue)])>,
    preserve_existing_rows: bool,
) -> io::Result<RebuiltTable> {
    let old_context = table.context();
    // The row index's header and the TCINFO keep their slots (HID 0x20 and 0x40 in a table Outlook or this writer
    // made), or take them first in a new heap; their new values are written into them at the end.
    let (mut heap, index_header, user_root) = if preserve_existing_rows {
        let mut heap = HeapBuilder::from_pages(table.writer_heap_pages()?);
        // The TCINFO, the row index and an in-heap row matrix are written anew below, so the old row index nodes and
        // rows are freed; kept, they would stay in the heap unreferenced, and a table rebuilt per folder creation would
        // grow with the square of its rows. The TCINFO and the index header are overwritten in place.
        let user_root = table.writer_heap_user_root()?;
        let index_header = old_context.row_index();
        free_row_index_nodes(&mut heap, index_header)?;
        if let Some(rows) = old_context
            .rows()
            .filter(|rows| matches!(rows.id_type(), Ok(NodeIdType::HeapNode)))
        {
            heap.free(HeapId::from(u32::from(rows)))?;
        }
        let index_header = if u32::from(index_header) == 0 {
            reserve_bth_header(&mut heap)?
        } else {
            index_header
        };
        (heap, index_header, user_root)
    } else {
        let mut heap = HeapBuilder::new();
        let index_header = reserve_bth_header(&mut heap)?;
        let user_root = reserve_tcinfo(&mut heap, old_context)?;
        (heap, index_header, user_root)
    };
    let row_size = usize::from(old_context.end_existence_bitmap());
    let mut row_chunks: Vec<Vec<u8>> = Vec::new();
    let mut row_ids = Vec::new();
    let push_row = |row_chunks: &mut Vec<Vec<u8>>, row: Vec<u8>| -> io::Result<()> {
        if row_size == 0 || row_size > MAX_DATA_BLOCK_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "table rows do not fit in a Unicode PST data block",
            ));
        }
        if row_chunks
            .last()
            .is_none_or(|chunk| chunk.len() + row_size > MAX_DATA_BLOCK_SIZE)
        {
            row_chunks.push(Vec::with_capacity(MAX_DATA_BLOCK_SIZE));
        }
        row_chunks.last_mut().unwrap().extend(row);
        Ok(())
    };

    for old_row in table.rows_matrix() {
        let row_id = NodeId::from(u32::from(old_row.id()));
        if preserve_existing_rows
            && property_update
                .as_ref()
                .is_none_or(|(update_row, _)| *update_row != row_id)
        {
            push_row(&mut row_chunks, old_row.writer_data()?)?;
            row_ids.push(row_id);
            continue;
        }
        let mut cells = old_row
            .columns(old_context)?
            .into_iter()
            .zip(old_context.columns())
            .map(|(value, column)| match value {
                None => Ok(None),
                Some(TableRowColumnValue::Small(value)) => Ok(Some(TableCell::Value(value))),
                Some(TableRowColumnValue::Heap(id)) if u32::from(id) == 0 => {
                    Ok(Some(TableCell::Bytes(Vec::new())))
                } // present, empty
                Some(TableRowColumnValue::Node(id)) if u32::from(id) == 0 => Ok(None),
                Some(value @ TableRowColumnValue::Heap(_)) => table
                    .read_column(&value, column.prop_type())
                    .map(TableCell::Value)
                    .map(Some),
                Some(TableRowColumnValue::Node(node)) => Ok(Some(TableCell::Node(node))),
            })
            .collect::<io::Result<Vec<_>>>()?;
        if preserve_existing_rows {
            // This row is encoded again with new heap allocations for its values; free the old ones.
            for value in old_row.columns(old_context)?.into_iter().flatten() {
                if let TableRowColumnValue::Heap(id) = value {
                    if u32::from(id) != 0 {
                        heap.free(id)?;
                    }
                }
            }
            for (column, cell) in old_context.columns().iter().zip(&mut cells) {
                if matches!(column.prop_id(), 0x0E30 | 0x0E33 | 0x0E34) {
                    *cell = None;
                }
            }
        }
        let mut unique = old_row.unique();
        if let Some((update_row, updates)) = &property_update {
            if row_id == *update_row {
                unique = row_version;
                for (column, cell) in old_context.columns().iter().zip(&mut cells) {
                    if let Some((_, value)) = updates.iter().find(|(property, value)| {
                        column.prop_id() == *property
                            && column.prop_type() == PropertyType::from(value)
                    }) {
                        *cell = Some(TableCell::Value(value.clone()));
                    } else if column.prop_id() == LTP_ROW_VERSION_PROP_ID
                        && column.prop_type() == PropertyType::Integer32
                    {
                        *cell = Some(TableCell::Value(PropertyValue::Integer32(
                            row_version as i32,
                        )));
                    }
                }
            }
        }
        let row = encode_table_row(old_context, &mut heap, row_id, unique, cells)?;
        push_row(&mut row_chunks, row)?;
        row_ids.push(row_id);
    }

    for (row_id, cells) in new_rows {
        let row = encode_table_row(old_context, &mut heap, row_id, row_version, cells)?;
        push_row(&mut row_chunks, row)?;
        row_ids.push(row_id);
    }

    let row_index = build_row_index(&mut heap, &row_ids, index_header)?;
    let rows_node = if row_ids.is_empty() {
        None
    } else {
        Some(rows_node.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "table rows node is missing")
        })?)
    };
    let context = TableContextInfo::new(
        old_context.end_4byte_values(),
        old_context.end_2byte_values(),
        old_context.end_1byte_values(),
        old_context.end_existence_bitmap(),
        row_index,
        rows_node,
        old_context.columns().to_vec(),
    )?;
    let mut context_data = Vec::new();
    context.write(&mut context_data)?;
    heap.replace(user_root, context_data)?;
    let heap_pages = heap.finish(HeapNodeType::Table, user_root)?;
    Ok((heap_pages, row_chunks, row_ids))
}

type NewTableRows = Vec<(NodeId, Vec<Option<TableCell>>)>;
/// Contents cells in subnodes: (node, the message's position in the batch, the column's property ID, the value).
type ExternalValues = Vec<(NodeId, usize, u16, Vec<u8>)>;

#[allow(clippy::too_many_arguments)]
fn new_contents_rows(
    context: &TableContextInfo,
    inputs: &[UnicodePstBatchMessage<'_>],
    store_record_key: [u8; 16],
    folder: NodeId,
    messages: &[NodeId],
    message_sizes: &[i32],
    mut next_subnode_index: u32,
    row_version: u32,
    names: &NameIds,
) -> io::Result<(NewTableRows, ExternalValues)> {
    let mut external = Vec::new();
    let rows = inputs
        .iter()
        .zip(messages)
        .zip(message_sizes)
        .enumerate()
        .map(|(position, ((input, message), message_size))| {
            let mut cells = context
                .columns()
                .iter()
                .map(|column| {
                    new_message_table_cell(
                        column,
                        &input.message,
                        input.attachments,
                        store_record_key,
                        folder,
                        *message,
                        row_version,
                        *message_size,
                        names,
                    )
                })
                .collect::<io::Result<Vec<_>>>()?;
            // Outlook only follows a table cell into a subnode of type NID_TYPE_LTP; an internal NID makes GetTable fail.
            for (column, cell) in context.columns().iter().zip(cells.iter_mut()) {
                if let Some(cell) = cell {
                    let mut spilled = Vec::new();
                    spill_table_cell(
                        cell,
                        column.prop_type(),
                        &mut next_subnode_index,
                        &mut spilled,
                    )?;
                    external.extend(
                        spilled
                            .into_iter()
                            .map(|(node, data)| (node, position, column.prop_id(), data)),
                    );
                }
            }
            Ok((*message, cells))
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok((rows, external))
}

fn bbt_entry(
    bbt_entries: &dyn BlockLookup,
    block: UnicodeBlockId,
) -> io::Result<UnicodeBlockBTreeEntry> {
    bbt_entries
        .lookup_block(block.search_key())?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("referenced PST block {:X} is missing", block.search_key()),
            )
        })
}

/// The leaf data blocks of a data tree, in order, as `(block, size)`.
fn data_tree_leaves(
    file: &mut File,
    bbt_entries: &dyn BlockLookup,
    block: UnicodeBlockId,
) -> io::Result<Vec<(UnicodeBlockId, u32)>> {
    let entry = bbt_entry(bbt_entries, block)?;
    match UnicodeDataTree::read(file, bbt_entries.crypt(), &entry)? {
        DataTree::Leaf(_) => Ok(vec![(block, u32::from(entry.size()))]),
        DataTree::Intermediate(tree) => {
            let mut leaves = Vec::new();
            for child in tree.entries() {
                leaves.extend(data_tree_leaves(file, bbt_entries, child.block())?);
            }
            Ok(leaves)
        }
    }
}

fn read_data_block(
    file: &mut File,
    bbt_entries: &dyn BlockLookup,
    block: UnicodeBlockId,
) -> io::Result<Vec<u8>> {
    let entry = bbt_entry(bbt_entries, block)?;
    match UnicodeDataTree::read(file, bbt_entries.crypt(), &entry)? {
        DataTree::Leaf(block) => Ok(block.data().to_vec()),
        DataTree::Intermediate(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a leaf data block",
        )),
    }
}

/// A data tree read along paths only. Opened only when it has the shape this writer writes, which `open` proves with
/// O(1) reads: every leaf but the last holds MAX_DATA_BLOCK_SIZE bytes, one XBLOCK holds up to 1,021 leaves, every
/// XBLOCK under an XXBLOCK but the last holds exactly 1,021, and there are at most two levels. Nothing per leaf is
/// kept.
struct DataTreePath {
    root: UnicodeBlockId,
    /// 0: the root is the only leaf; 1: an XBLOCK; 2: an XXBLOCK.
    level: u8,
    total: u64,
    leaves: usize,
    /// The root's children (none for a single leaf); at most 1,021.
    children: Vec<UnicodeBlockId>,
    /// The XBLOCK under an XXBLOCK read last, by its position: the only one kept.
    inner: Option<(usize, Vec<UnicodeBlockId>)>,
}

impl DataTreePath {
    const FANOUT: usize = 1021;

    fn read_index(
        file: &mut File,
        bbt: &dyn BlockLookup,
        block: UnicodeBlockId,
        level: u8,
    ) -> io::Result<(u64, Vec<UnicodeBlockId>)> {
        crate::measurement::count_op!(DATA_TREE_INDEX_BLOCKS_READ, 1);
        let entry = bbt_entry(bbt, block)?;
        match UnicodeDataTree::read(file, bbt.crypt(), &entry)? {
            DataTree::Intermediate(tree) if tree.header().level() == level => Ok((
                u64::from(tree.header().total_size()),
                tree.entries().iter().map(|entry| entry.block()).collect(),
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data tree index block has another level",
            )),
        }
    }

    /// `None` when the tree does not have the shape described above.
    fn open(
        file: &mut File,
        bbt: &dyn BlockLookup,
        root: UnicodeBlockId,
    ) -> io::Result<Option<Self>> {
        let full = MAX_DATA_BLOCK_SIZE as u64;
        let entry = bbt_entry(bbt, root)?;
        let (level, total, children) = match UnicodeDataTree::read(file, bbt.crypt(), &entry)? {
            DataTree::Leaf(_) => {
                return Ok(Some(Self {
                    root,
                    level: 0,
                    total: u64::from(entry.size()),
                    leaves: 1,
                    children: Vec::new(),
                    inner: None,
                }));
            }
            DataTree::Intermediate(tree) => (
                tree.header().level(),
                u64::from(tree.header().total_size()),
                tree.entries()
                    .iter()
                    .map(|entry| entry.block())
                    .collect::<Vec<_>>(),
            ),
        };
        crate::measurement::count_op!(DATA_TREE_INDEX_BLOCKS_READ, 1);
        let leaves =
            usize::try_from(total.div_ceil(full)).map_err(|_| PstError::IntegerConversion)?;
        let mut path = Self {
            root,
            level,
            total,
            leaves,
            children,
            inner: None,
        };
        // The last leaf's size: with it, the counts and totals below force every other leaf (and every other XBLOCK)
        // to be full, because no leaf exceeds a full block and no XBLOCK exceeds 1,021 leaves.
        let last_leaf = match level {
            1 if leaves >= 1 && path.children.len() == leaves => path.children[leaves - 1],
            2 if leaves > Self::FANOUT && path.children.len() == leaves.div_ceil(Self::FANOUT) => {
                let group = path.children.len() - 1;
                let (inner_total, inner) = Self::read_index(file, bbt, path.children[group], 1)?;
                if inner.len() != leaves - group * Self::FANOUT
                    || inner_total != total - (group * Self::FANOUT) as u64 * full
                {
                    return Ok(None);
                }
                let last = inner[inner.len() - 1];
                path.inner = Some((group, inner));
                last
            }
            _ => return Ok(None),
        };
        let last_size = u64::from(bbt_entry(bbt, last_leaf)?.size());
        if total == 0 || last_size == 0 || last_size != total - (leaves as u64 - 1) * full {
            return Ok(None);
        }
        Ok(Some(path))
    }

    fn last_size(&self) -> u64 {
        self.total - (self.leaves as u64 - 1) * MAX_DATA_BLOCK_SIZE as u64
    }

    /// The leaves of group `group` (1,021 leaves per group; a tree of one level has only group 0).
    fn group(
        &mut self,
        file: &mut File,
        bbt: &dyn BlockLookup,
        group: usize,
    ) -> io::Result<Vec<UnicodeBlockId>> {
        match self.level {
            0 if group == 0 => Ok(vec![self.root]),
            1 if group == 0 => Ok(self.children.clone()),
            2 if group < self.children.len() => {
                if self.inner.as_ref().is_none_or(|(at, _)| *at != group) {
                    let expected = if group + 1 == self.children.len() {
                        self.leaves - group * Self::FANOUT
                    } else {
                        Self::FANOUT
                    };
                    let (_, inner) = Self::read_index(file, bbt, self.children[group], 1)?;
                    if inner.len() != expected {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "data tree index block has an unexpected number of leaves",
                        ));
                    }
                    self.inner = Some((group, inner));
                }
                Ok(self.inner.as_ref().expect("read above").1.clone())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data tree leaf is out of range",
            )),
        }
    }

    fn leaf(
        &mut self,
        file: &mut File,
        bbt: &dyn BlockLookup,
        index: usize,
    ) -> io::Result<UnicodeBlockId> {
        if index >= self.leaves {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data tree leaf is out of range",
            ));
        }
        if self.level == 2 {
            let group = index / Self::FANOUT;
            self.group(file, bbt, group)?;
            return Ok(self.inner.as_ref().expect("read above").1[index % Self::FANOUT]);
        }
        Ok(self.group(file, bbt, 0)?[index])
    }
}

/// Replace and append leaves of a data tree along the changed paths only: the new leaves, the index blocks over
/// them (one XBLOCK per touched group of 1,021 leaves, and the XXBLOCK), nothing else. `edits` maps leaf positions to
/// their new data; positions from the tree's leaf count on are appends and must follow without a gap, and when the
/// tree grows its old last leaf must be among the edits (it is no longer last). Every leaf but the new last one must
/// be full. Every reference the new tree adds or drops is stated in `tx.changes` (as `append_to_data_tree` does), so the
/// reference counting walks no unchanged child. Returns the new root; the caller's node or subnode change moves the
/// root's own reference.
fn edit_data_tree(
    file: &mut File,
    tx: &mut Tx<'_>,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    tree: &mut DataTreePath,
    edits: BTreeMap<usize, Vec<u8>>,
) -> io::Result<UnicodeBlockId> {
    const FANOUT: usize = DataTreePath::FANOUT;
    let full = MAX_DATA_BLOCK_SIZE;
    let old_n = tree.leaves;
    let Some(&last_edit) = edits.keys().next_back() else {
        return Ok(tree.root);
    };
    let new_n = old_n.max(last_edit + 1);
    if new_n > FANOUT * FANOUT {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the data tree exceeds two levels of 1,021 entries",
        ));
    }
    if (old_n..new_n).any(|index| !edits.contains_key(&index))
        || (new_n > old_n && !edits.contains_key(&(old_n - 1)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a data tree edit must append without a gap and rewrite the old last leaf",
        ));
    }
    if edits.iter().any(|(index, data)| {
        data.is_empty() || data.len() > full || (*index + 1 < new_n && data.len() != full)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "every data tree leaf but the last must be a full block",
        ));
    }
    let last_len = match edits.get(&(new_n - 1)) {
        Some(data) => data.len() as u64,
        None => tree.last_size(),
    };
    let mut new_ids = BTreeMap::new();
    for (index, data) in edits {
        new_ids.insert(
            index,
            push_block(
                blocks,
                next_bid,
                next_offset,
                false,
                NewBlockData::Data(data),
            )?,
        );
    }
    if new_n == 1 {
        return Ok(new_ids[&0]);
    }
    // Every new leaf is the child of a new index block.
    for id in new_ids.values() {
        tx.changes.explicit.push((*id, 1));
    }
    let leaf_size = |index: usize| {
        if index + 1 == new_n {
            last_len
        } else {
            full as u64
        }
    };
    let span_total = |range: std::ops::Range<usize>| {
        u32::try_from(range.map(leaf_size).sum::<u64>()).map_err(|_| PstError::IntegerConversion)
    };
    let old_groups = if tree.level == 2 {
        tree.children.len()
    } else {
        1
    };
    let groups = new_n.div_ceil(FANOUT);
    let mut top: Vec<UnicodeBlockId> = Vec::with_capacity(groups);
    for group in 0..groups {
        let range = group * FANOUT..((group + 1) * FANOUT).min(new_n);
        let touched = group >= old_groups || new_ids.range(range.clone()).next().is_some();
        if !touched {
            // Unchanged and complete (appends to a group always touch it): kept with its ID.
            match tree.level {
                2 => top.push(tree.children[group]),
                // The old root XBLOCK becomes the first child of the new XXBLOCK.
                1 => {
                    tx.changes.explicit.push((tree.root, 1));
                    top.push(tree.root);
                }
                _ => {
                    return Err(io::Error::other(
                        "a single-leaf data tree has no complete group",
                    ))
                }
            }
            continue;
        }
        let old_leaves = if group < old_groups {
            tree.group(file, tx, group)?
        } else {
            Vec::new()
        };
        let entries = range
            .clone()
            .map(|index| {
                new_ids
                    .get(&index)
                    .copied()
                    .unwrap_or_else(|| old_leaves[index - group * FANOUT])
            })
            .collect::<Vec<_>>();
        if group < old_groups {
            match tree.level {
                0 => {
                    // The old single leaf, if kept, gains this XBLOCK as its parent; if replaced, the node change drops it.
                    if !new_ids.contains_key(&0) {
                        tx.changes.explicit.push((tree.root, 1));
                    }
                }
                level => {
                    // Replaced leaves lose the old XBLOCK; the old XBLOCK is released without walking its children, the
                    // kept ones of which the new XBLOCK takes over.
                    for (index, _) in new_ids
                        .range(range.clone())
                        .filter(|(index, _)| **index < old_n)
                    {
                        tx.changes
                            .explicit
                            .push((old_leaves[*index - group * FANOUT], -1));
                    }
                    let old_index = if level == 2 {
                        tree.children[group]
                    } else {
                        tree.root
                    };
                    if level == 2 {
                        tx.changes.explicit.push((old_index, -1));
                    }
                    tx.changes.released_handled.insert(old_index.search_key());
                }
            }
        }
        let size = span_total(range)?;
        let id = push_block(
            blocks,
            next_bid,
            next_offset,
            true,
            NewBlockData::DataTree(
                entries
                    .into_iter()
                    .map(UnicodeDataTreeEntry::from)
                    .collect(),
                1,
                size,
            ),
        )?;
        tx.changes.precounted.insert(id.search_key());
        if groups == 1 {
            return Ok(id);
        }
        tx.changes.explicit.push((id, 1));
        top.push(id);
    }
    if tree.level == 2 {
        tx.changes.released_handled.insert(tree.root.search_key());
    }
    let size = span_total(0..new_n)?;
    let id = push_block(
        blocks,
        next_bid,
        next_offset,
        true,
        NewBlockData::DataTree(
            top.into_iter().map(UnicodeDataTreeEntry::from).collect(),
            2,
            size,
        ),
    )?;
    tx.changes.precounted.insert(id.search_key());
    Ok(id)
}

/// What a writer session keeps between commits about one folder's contents table: the roots of its heap and row-matrix
/// data trees as the last commit to the folder left them, the row count, the last row ID and the verified unread count.
/// Nothing per row or per page: each append reads the pages and the row block it changes from disk (`ContentsAppend`)
/// and drops them after the commit.
pub(crate) struct ContentsState {
    row_count: usize,
    last_id: Option<u32>,
    pub(crate) unread: i32,
    pub(crate) heap_root: UnicodeBlockId,
    pub(crate) rows_root: UnicodeBlockId,
}

/// Whether the hierarchy-table path edits are taken. Always in a product build; a test can turn them off to exercise
/// the full-rebuild fallbacks.
fn hierarchy_path_edits() -> bool {
    #[cfg(test)]
    if crate::session::FORCE_TABLE_REBUILD.with(std::cell::Cell::get) {
        return false;
    }
    true
}

/// The full-rebuild fallback of a hierarchy row patch, which leaves no unreferenced allocations in the heap: the table
/// with `row`'s cells set, its rows written into its row-matrix subnode.
#[allow(clippy::too_many_arguments)]
fn rebuild_hierarchy_row(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    table: &dyn TableContext,
    subnodes: &[UnicodeLeafSubNodeTreeEntry],
    row: NodeId,
    cells: &[(u16, PropertyValue)],
    row_version: u32,
) -> io::Result<PatchedTable> {
    let rows_node = table_rows_node(table.context(), subnodes)?;
    let (pages, rows, _) = rebuild_table(
        table,
        vec![],
        Some(rows_node),
        row_version,
        Some((row, cells)),
        true,
    )?;
    let heap = push_data_tree_chunks(blocks, next_bid, next_offset, pages)?;
    let rows = push_data_tree_chunks(blocks, next_bid, next_offset, rows)?;
    Ok((heap, Some((rows_node, rows))))
}

/// The hierarchy table's node entry after a row patch or rebuild: the new heap root, and a new subnode tree when the
/// row-matrix subnode changed.
fn hierarchy_entry_after(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    node: NodeId,
    entry: UnicodeNodeBTreeEntry,
    mut subnodes: Vec<UnicodeLeafSubNodeTreeEntry>,
    (heap, rows): PatchedTable,
) -> io::Result<UnicodeNodeBTreeEntry> {
    let sub_node = match rows {
        Some((rows_node, rows)) => {
            subnodes.retain(|entry| entry.node() != rows_node);
            subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None));
            Some(push_subnode_tree(blocks, next_bid, next_offset, subnodes)?)
        }
        None => entry.sub_node(),
    };
    Ok(UnicodeNodeBTreeEntry::new(
        node,
        heap,
        sub_node,
        entry.parent(),
    ))
}

/// One commit's view of a contents table for appending: its context, the heap's user root, both data trees as paths,
/// and a heap builder that holds only the pages this append loads.
pub(crate) struct ContentsAppend {
    pub(crate) context: TableContextInfo,
    user_root: HeapId,
    heap_tree: DataTreePath,
    rows_tree: DataTreePath,
    heap: HeapBuilder,
}

/// Load heap page `page` into `heap` unless it is loaded; a non-final page must be a full block.
fn ensure_heap_page(
    file: &mut File,
    bbt: &dyn BlockLookup,
    tree: &mut DataTreePath,
    heap: &mut HeapBuilder,
    page: usize,
) -> io::Result<()> {
    if heap.is_loaded(page) {
        return Ok(());
    }
    let leaf = tree.leaf(file, bbt, page)?;
    let data = read_data_block(file, bbt, leaf)?;
    if page + 1 < tree.leaves && data.len() != MAX_DATA_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a non-final heap page is not a full block",
        ));
    }
    crate::measurement::count_op!(CONTENTS_PAGES_LOADED, 1);
    heap.load_levels(page, &data)?;
    heap.load(page, decode_heap_page(page, &data)?);
    Ok(())
}

/// Load the header pages that store the fill levels of `heap`'s dirty pages, so `encode_dirty` can write them.
fn ensure_level_pages(
    file: &mut File,
    bbt: &dyn BlockLookup,
    tree: &mut DataTreePath,
    heap: &mut HeapBuilder,
) -> io::Result<()> {
    for page in heap.level_pages() {
        if page < tree.leaves {
            ensure_heap_page(file, bbt, tree, heap, page)?;
        }
    }
    Ok(())
}

/// Any table context read along paths: its context, the heap's user root, the heap data tree, the row-matrix data tree
/// when the rows are in a subnode, and a heap builder holding only the pages read so far.
pub(crate) struct TableView {
    pub(crate) context: TableContextInfo,
    user_root: HeapId,
    heap_tree: DataTreePath,
    rows_tree: Option<DataTreePath>,
    heap: HeapBuilder,
}

/// Open the table whose heap is `heap_root`; `rows_block` gives the block of its row-matrix subnode by NID. `None`
/// when a data tree does not have the shape this writer writes, the heap is not a table, or the row-matrix subnode is
/// missing.
fn open_table_view(
    file: &mut File,
    bbt: &dyn BlockLookup,
    heap_root: UnicodeBlockId,
    rows_block: impl FnOnce(NodeId) -> Option<UnicodeBlockId>,
) -> io::Result<Option<TableView>> {
    let Some(mut heap_tree) = DataTreePath::open(file, bbt, heap_root)? else {
        return Ok(None);
    };
    let first_leaf = heap_tree.leaf(file, bbt, 0)?;
    let first = read_data_block(file, bbt, first_leaf)?;
    let header = HeapNodeHeader::read(&mut Cursor::new(&first))?;
    if header.client_signature() != HeapNodeType::Table {
        return Ok(None);
    }
    let user_root = header.user_root();
    let mut heap = HeapBuilder::paged(heap_tree.leaves);
    crate::measurement::count_op!(CONTENTS_PAGES_LOADED, 1);
    heap.load_levels(0, &first)?;
    heap.load(0, decode_heap_page(0, &first)?);
    ensure_heap_page(
        file,
        bbt,
        &mut heap_tree,
        &mut heap,
        usize::from(user_root.block_index()),
    )?;
    let context = TableContextInfo::read(&mut heap.get(user_root)?)?;
    let rows_tree = match context.rows() {
        Some(rows) if !matches!(rows.id_type(), Ok(NodeIdType::HeapNode)) => {
            let Some(block) = rows_block(rows) else {
                return Ok(None);
            };
            let Some(tree) = DataTreePath::open(file, bbt, block)? else {
                return Ok(None);
            };
            Some(tree)
        }
        _ => None,
    };
    Ok(Some(TableView {
        context,
        user_root,
        heap_tree,
        rows_tree,
        heap,
    }))
}

/// Open the append view of the table `state` describes. `None` when its layout needs the full rebuild (another tree
/// shape, rows in the heap, an unusable row index).
fn open_contents_append(
    file: &mut File,
    bbt: &dyn BlockLookup,
    state: &ContentsState,
) -> io::Result<Option<ContentsAppend>> {
    let Some(view) = open_table_view(file, bbt, state.heap_root, |_| Some(state.rows_root))? else {
        return Ok(None);
    };
    let TableView {
        context,
        user_root,
        mut heap_tree,
        rows_tree,
        mut heap,
    } = view;
    let row_size = usize::from(context.end_existence_bitmap());
    let Some(rows_tree) = rows_tree.filter(|_| row_size != 0 && row_size <= MAX_DATA_BLOCK_SIZE)
    else {
        return Ok(None);
    };
    ensure_heap_page(
        file,
        bbt,
        &mut heap_tree,
        &mut heap,
        usize::from(context.row_index().block_index()),
    )?;
    let index = HeapTreeHeader::read(&mut heap.get(context.row_index())?)?;
    if index.key_size() != 4 || index.entry_size() != 4 || u32::from(index.root()) == 0 {
        return Ok(None);
    }
    Ok(Some(ContentsAppend {
        context,
        user_root,
        heap_tree,
        rows_tree,
        heap,
    }))
}

/// The position of row `row` in the row matrix, through the row-index BTH (MS-PST 2.3.4.3), reading only the heap pages
/// on its path. `None` when the index is empty or does not hold the row.
fn find_table_row(
    file: &mut File,
    bbt: &dyn BlockLookup,
    view: &mut TableView,
    row: u32,
) -> io::Result<Option<usize>> {
    let index_hid = view.context.row_index();
    if u32::from(index_hid) == 0 {
        return Ok(None);
    }
    ensure_heap_page(
        file,
        bbt,
        &mut view.heap_tree,
        &mut view.heap,
        usize::from(index_hid.block_index()),
    )?;
    let index = HeapTreeHeader::read(&mut view.heap.get(index_hid)?)?;
    if index.key_size() != 4 || index.entry_size() != 4 {
        return Ok(None);
    }
    let mut node = index.root();
    if u32::from(node) == 0 {
        return Ok(None);
    }
    for level in (0..=index.levels()).rev() {
        ensure_heap_page(
            file,
            bbt,
            &mut view.heap_tree,
            &mut view.heap,
            usize::from(node.block_index()),
        )?;
        let data = view.heap.get(node)?;
        if data.is_empty() || data.len() % 8 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "table row index node is malformed",
            ));
        }
        let mut records = data.chunks(8).map(|record| {
            (
                u32::from_le_bytes(record[..4].try_into().unwrap()),
                u32::from_le_bytes(record[4..].try_into().unwrap()),
            )
        });
        if level > 0 {
            // An intermediate record's key is the smallest key under it (2.3.2.2): the last one not above `row`.
            match records.filter(|(key, _)| *key <= row).last() {
                Some((_, child)) => node = HeapId::from(child),
                None => return Ok(None),
            }
        } else {
            return records
                .find(|(key, _)| *key == row)
                .map(|(_, position)| {
                    usize::try_from(position).map_err(|_| PstError::IntegerConversion.into())
                })
                .transpose();
        }
    }
    Ok(None)
}

/// The session state of `table` from a full read of it (the first append to a folder in a session; O(rows) once).
/// Returns `None` when the table layout (rows in the heap, empty table) needs the full rebuild instead.
fn load_contents_state(
    table: &dyn TableContext,
    heap_block: UnicodeBlockId,
    rows_block: Option<UnicodeBlockId>,
    unread: i32,
) -> io::Result<Option<ContentsState>> {
    let context = table.context();
    let row_size = usize::from(context.end_existence_bitmap());
    let old_count = table.rows_matrix().count();
    let last_id = table.rows_matrix().map(|row| u32::from(row.id())).max();
    let Some(rows_block) = rows_block else {
        return Ok(None);
    };
    if old_count == 0
        || row_size == 0
        || row_size > MAX_DATA_BLOCK_SIZE
        || context
            .rows()
            .is_none_or(|rows| matches!(rows.id_type(), Ok(NodeIdType::HeapNode)))
    {
        return Ok(None);
    }
    Ok(Some(ContentsState {
        row_count: old_count,
        last_id,
        unread,
        heap_root: heap_block,
        rows_root: rows_block,
    }))
}

/// Append rows to a table in place: unchanged heap pages and row blocks keep their block IDs; only the last row block,
/// the dirty heap pages and the index blocks over them are written (along the changed paths of both data trees), so a
/// call writes and reads O(new rows) bytes. Returns `None`, leaving `new_rows` and `state` untouched, when the new row
/// IDs are not above the existing ones or the layout needs the full rebuild. On success `state` describes the table as
/// written.
#[allow(clippy::too_many_arguments)]
fn append_rows_to_state(
    file: &mut File,
    tx: &mut Tx<'_>,
    state: &mut ContentsState,
    view: Option<ContentsAppend>,
    new_rows: &mut NewTableRows,
    row_version: u32,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
) -> io::Result<Option<(UnicodeBlockId, UnicodeBlockId, usize)>> {
    let mut last_id = state.last_id;
    let in_order = new_rows.iter().all(|(id, _)| {
        let id = u32::from(*id);
        let ordered = last_id.is_none_or(|last| id > last);
        last_id = Some(id);
        ordered
    });
    if !in_order {
        return Ok(None);
    }
    let view = match view {
        Some(view) => view,
        None => match open_contents_append(file, tx, state)? {
            Some(view) => view,
            None => return Ok(None),
        },
    };
    let ContentsAppend {
        context,
        user_root,
        mut heap_tree,
        mut rows_tree,
        mut heap,
    } = view;
    let row_size = usize::from(context.end_existence_bitmap());
    let rows_per_block = MAX_DATA_BLOCK_SIZE / row_size;
    let old_count = state.row_count;
    let tail_rows = old_count.checked_sub((rows_tree.leaves - 1) * rows_per_block);
    let Some(tail_rows) = tail_rows.filter(|rows| (1..=rows_per_block).contains(rows)) else {
        return Ok(None);
    };
    let tail_block = rows_tree.leaf(file, tx, rows_tree.leaves - 1)?;
    let mut tail = read_data_block(file, tx, tail_block)?;
    if tail.len() < tail_rows * row_size {
        return Ok(None);
    }
    tail.truncate(tail_rows * row_size);

    // The pages this append reads or changes: the row index header (loaded by the view), the right spine of the row
    // index and the last page, where new allocations go.
    let index_hid = context.row_index();
    let index = HeapTreeHeader::read(&mut heap.get(index_hid)?)?;
    let mut node = index.root();
    for level in (0..=index.levels()).rev() {
        ensure_heap_page(
            file,
            tx,
            &mut heap_tree,
            &mut heap,
            usize::from(node.block_index()),
        )?;
        if level > 0 {
            let data = heap.get(node)?;
            if data.is_empty() || data.len() % 8 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "table row index node is malformed",
                ));
            }
            node = HeapId::from(u32::from_le_bytes(
                data[data.len() - 4..].try_into().unwrap(),
            ));
        }
    }
    let last_page = heap.count - 1;
    ensure_heap_page(file, tx, &mut heap_tree, &mut heap, last_page)?;

    let spine = take_row_index_spine(&mut heap, &index)?;
    let mut records = Vec::with_capacity(new_rows.len());
    for (row_id, cells) in std::mem::take(new_rows) {
        let row_index =
            u32::try_from(old_count + records.len()).map_err(|_| PstError::IntegerConversion)?;
        tail.extend(encode_table_row(
            &context,
            &mut heap,
            row_id,
            row_version,
            cells,
        )?);
        records.push((u32::from(row_id), row_index));
    }
    let row_count = old_count + records.len();
    let header = append_row_index_spine(&mut heap, spine, index.levels(), &records)?;
    heap.replace(index_hid, header)?;

    ensure_level_pages(file, tx, &mut heap_tree, &mut heap)?;
    let pages = heap.encode_dirty(HeapNodeType::Table, user_root)?;
    let heap_root = edit_data_tree(
        file,
        tx,
        blocks,
        next_bid,
        next_offset,
        &mut heap_tree,
        pages.into_iter().collect(),
    )?;

    let mut chunks = tail
        .chunks(rows_per_block * row_size)
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    let last = chunks.len() - 1;
    for chunk in &mut chunks[..last] {
        chunk.resize(MAX_DATA_BLOCK_SIZE, 0);
    }
    let first = rows_tree.leaves - 1;
    let edits = chunks
        .into_iter()
        .enumerate()
        .map(|(offset, chunk)| (first + offset, chunk))
        .collect();
    let rows_root = edit_data_tree(
        file,
        tx,
        blocks,
        next_bid,
        next_offset,
        &mut rows_tree,
        edits,
    )?;
    state.row_count = row_count;
    state.last_id = last_id;
    state.heap_root = heap_root;
    state.rows_root = rows_root;
    Ok(Some((heap_root, rows_root, row_count)))
}

/// A one-page property context with every value in the heap: a folder's (`folder_data`). Attachments use
/// `property_context_pages`, which puts a value over 3,580 bytes in a subnode.
fn property_context_data(
    properties: &[(u16, PropertyType, Option<Vec<u8>>, u32)],
) -> io::Result<Vec<u8>> {
    let mut allocations = vec![vec![], vec![], vec![]];
    let mut records = Vec::with_capacity(properties.len());

    for (prop_id, prop_type, value, small) in properties {
        let record = if let Some(value) = value {
            let allocation = allocations.len();
            allocations.push(value.clone());
            PropertyTreeRecord::new(
                *prop_id,
                *prop_type,
                PropertyValueRecord::Heap(heap_id(allocation)?),
            )
        } else {
            PropertyTreeRecord::new(*prop_id, *prop_type, PropertyValueRecord::Small(*small))
        };
        record.write(&mut records)?;
    }

    let tree_root = heap_id(2)?;
    let tree_header = HeapTreeHeader::new(2, 6, 0, tree_root)?;
    tree_header.write(&mut allocations[1])?;
    allocations[2] = records;

    finish_heap(allocations, HeapNodeType::Properties, heap_id(1)?)
}

fn table_context_data(row_ids: &[NodeId]) -> io::Result<Vec<u8>> {
    // Allocation 1 (HID 0x20) is the row index's BTH header and allocation 2 (0x40) the TCINFO, as Outlook writes every
    // table; the row index leaf and the rows follow.
    let row_index = heap_id(1)?;
    let rows_heap = (!row_ids.is_empty()).then(|| heap_id(4)).transpose()?;
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let context = TableContextInfo::new(
        8,
        8,
        8,
        9,
        row_index,
        rows_heap.map(|id| NodeId::from(u32::from(id))),
        columns,
    )?;

    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;

    let tree_root = if row_ids.is_empty() {
        HeapId::from(0)
    } else {
        heap_id(3)?
    };
    let tree_header = HeapTreeHeader::new(4, 4, 0, tree_root)?;
    let mut tree_header_bytes = Vec::new();
    tree_header.write(&mut tree_header_bytes)?;

    let mut index_bytes = Vec::new();
    let mut rows = Vec::new();
    for (index, node_id) in row_ids.iter().enumerate() {
        let row_id = TableRowId::new(u32::from(*node_id));
        row_id.write(&mut index_bytes)?;
        index_bytes.write_u32::<byteorder::LittleEndian>(
            u32::try_from(index).map_err(|_| PstError::IntegerConversion)?,
        )?;

        rows.write_u32::<byteorder::LittleEndian>(u32::from(*node_id))?;
        rows.write_u32::<byteorder::LittleEndian>(0)?;
        rows.push(0xC0);
    }

    let mut allocations = vec![vec![], tree_header_bytes, context_bytes];
    if !row_ids.is_empty() {
        allocations.push(index_bytes);
        allocations.push(rows);
    }
    finish_heap(allocations, HeapNodeType::Table, heap_id(2)?)
}

/// A rebuilt table: its heap pages, and when it has rows, the row matrix's subnode with its blocks and the subnodes of
/// cells too large for the heap, all in the table's own subnode tree.
type RebuiltRecipientTable = (
    Vec<Vec<u8>>,
    Option<(NodeId, Vec<Vec<u8>>, Vec<(NodeId, Vec<u8>)>)>,
);

fn one_off_entry_id(recipient: &UnicodePstRecipient<'_>) -> Vec<u8> {
    let mut entry_id = vec![0; 4];
    entry_id.extend_from_slice(&[
        0x81, 0x2B, 0x1F, 0xA4, 0xBE, 0xA3, 0x10, 0x19, 0x9D, 0x6E, 0x00, 0xDD, 0x01, 0x0F, 0x54,
        0x02,
    ]);
    entry_id.extend_from_slice(&0u16.to_le_bytes());
    entry_id.extend_from_slice(&0x8000u16.to_le_bytes());
    entry_id.extend(utf16z_bytes(if recipient.name.is_empty() {
        recipient.email
    } else {
        recipient.name
    }));
    entry_id.extend(utf16z_bytes("SMTP"));
    entry_id.extend(utf16z_bytes(recipient.email));
    entry_id
}

fn recipient_search_key(email: &str) -> Vec<u8> {
    let mut key = format!("SMTP:{email}").to_ascii_uppercase().into_bytes();
    key.push(0);
    key
}

fn recipient_table_data(
    recipients: &[UnicodePstRecipient<'_>],
    row_version: u32,
) -> io::Result<RebuiltRecipientTable> {
    // The fixed columns: (type, ID, offset, size, existence bitmap index). Their layout is the same with or without
    // given properties: a table without them is unchanged, byte for byte.
    let mut columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0C15, 8, 4, 2),
        TableColumnDescriptor::new(PropertyType::Boolean, 0x0E0F, 52, 1, 3),
        TableColumnDescriptor::new(PropertyType::Binary, 0x0FF9, 12, 4, 4),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0FFE, 16, 4, 5),
        TableColumnDescriptor::new(PropertyType::Binary, 0x0FFF, 20, 4, 6),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3001, 24, 4, 7),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3002, 28, 4, 8),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3003, 32, 4, 9),
        TableColumnDescriptor::new(PropertyType::Binary, 0x300B, 36, 4, 10),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x3900, 40, 4, 11),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x39FE, 44, 4, 12),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x39FF, 48, 4, 13),
        TableColumnDescriptor::new(PropertyType::Boolean, 0x3A40, 53, 1, 14),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let (mut end_4b, mut end_1b, mut end_bitmap) = (52_u16, 54_u16, 56_u16);
    // A column for every tag a recipient gives (`check_message` makes one tag one type). MS-PST 2.3.4.4.1: 4- and
    // 8-byte cells, then 2-byte, then 1-byte cells, then the cell existence bitmap. The given 4-byte cells follow the
    // fixed ones, the 8-byte ones follow them, and the 1-byte cells move after them, with no padding, as in every table
    // Outlook writes (an 8-byte cell at offset 60 or 220 in Outlook's own). Putting the 8-byte cells at an 8-byte
    // boundary leaves 4 bytes of the row to no column when the 4-byte cells end at one of 4 mod 8; SCANPST then finds
    // errors and its repair deletes the recipients.
    let mut given: BTreeMap<u16, PropertyType> = BTreeMap::new();
    for recipient in recipients {
        for property in recipient.properties {
            given.insert(property.id, property.value.prop_type());
        }
    }
    if !given.is_empty() {
        let size = |prop_type: PropertyType| match prop_type {
            PropertyType::Boolean => 1_u16,
            PropertyType::Time => 8,
            _ => 4,
        };
        let mut index = 15_u8;
        let mut offset = 52_u16;
        let mut extra = Vec::new();
        for pass in [4_u16, 8] {
            for (id, prop_type) in given
                .iter()
                .filter(|(_, prop_type)| size(**prop_type) == pass)
            {
                extra.push(TableColumnDescriptor::new(
                    *prop_type, *id, offset, pass as u8, index,
                ));
                offset += pass;
                index = index.checked_add(1).ok_or(PstError::IntegerConversion)?;
            }
        }
        end_4b = offset;
        let mut one_byte = vec![
            (0x0E0F_u16, PropertyType::Boolean, 3_u8),
            (0x3A40, PropertyType::Boolean, 14),
        ];
        for (id, prop_type) in given.iter().filter(|(_, prop_type)| size(**prop_type) == 1) {
            one_byte.push((*id, *prop_type, index));
            index = index.checked_add(1).ok_or(PstError::IntegerConversion)?;
        }
        columns.retain(|column| column.prop_id() != 0x0E0F && column.prop_id() != 0x3A40);
        for (id, prop_type, bit) in one_byte {
            columns.push(TableColumnDescriptor::new(prop_type, id, offset, 1, bit));
            offset += 1;
        }
        end_1b = offset;
        end_bitmap = end_1b + u16::from(index).div_ceil(8);
        columns.extend(extra);
        // MS-PST 2.3.4.1: "The entries in this array MUST be sorted by the tag field of TCOLDESC". One ID has one type
        // here, so the ID orders them (the fixed list is in that order already, the row ID and version last).
        columns.sort_by_key(|column| column.prop_id());
    }
    let row_context = TableContextInfo::new(
        end_4b,
        end_4b,
        end_1b,
        end_bitmap,
        HeapId::default(),
        None,
        columns.clone(),
    )?;
    let row_ids = (1..=recipients.len())
        .map(|id| u32::try_from(id).map_err(|_| PstError::IntegerConversion.into()))
        .collect::<io::Result<Vec<_>>>()?;
    let mut heap = HeapBuilder::new();
    // The row index's header at HID 0x20, the TCINFO at 0x40.
    let index_header = reserve_bth_header(&mut heap)?;
    let user_root = reserve_tcinfo(&mut heap, &row_context)?;
    let (mut cell_subnodes, mut next_cell_subnode) = (Vec::new(), 0x81_u32);
    let rows = row_ids
        .iter()
        .zip(recipients)
        .map(|(row_id, recipient)| {
            let entry_id = one_off_entry_id(recipient);
            let display_name = display_name(recipient.name, recipient.email);
            let cells = columns
                .iter()
                .map(|column| -> io::Result<Option<TableCell>> {
                    Ok(match column.prop_id() {
                        0x0C15 => Some(TableCell::Value(PropertyValue::Integer32(
                            recipient.recipient_type as i32,
                        ))),
                        0x0E0F => Some(TableCell::Value(PropertyValue::Boolean(true))),
                        0x0FF9 => None,
                        0x0FFE => Some(TableCell::Value(PropertyValue::Integer32(6))),
                        0x0FFF => Some(TableCell::Bytes(entry_id.clone())),
                        0x3001 => string_table_cell(PropertyType::Unicode, display_name),
                        0x3002 => string_table_cell(PropertyType::Unicode, "SMTP"),
                        0x3003 => string_table_cell(PropertyType::Unicode, recipient.email),
                        0x300B => Some(TableCell::Bytes(recipient_search_key(recipient.email))),
                        0x3900 => Some(TableCell::Value(PropertyValue::Integer32(0))),
                        0x39FE => string_table_cell(PropertyType::Unicode, recipient.email),
                        0x39FF => string_table_cell(PropertyType::Unicode, display_name),
                        0x3A40 => Some(TableCell::Value(PropertyValue::Boolean(false))),
                        LTP_ROW_ID_PROP_ID => Some(TableCell::Value(PropertyValue::Integer32(
                            i32::try_from(*row_id).map_err(|_| PstError::IntegerConversion)?,
                        ))),
                        LTP_ROW_VERSION_PROP_ID => Some(TableCell::Value(
                            PropertyValue::Integer32(row_version as i32),
                        )),
                        // A given property's cell; an empty string or binary is HNID 0 with its existence bit set.
                        id => recipient
                            .properties
                            .iter()
                            .find(|property| property.id == id)
                            .map(|property| TableCell::Value(property.value.property_value())),
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            let mut cells = cells;
            // Cells too large for the heap go to subnodes after the row matrix's (0x80).
            for (column, cell) in columns.iter().zip(cells.iter_mut()) {
                if let Some(cell) = cell {
                    spill_table_cell(
                        cell,
                        column.prop_type(),
                        &mut next_cell_subnode,
                        &mut cell_subnodes,
                    )?;
                }
            }
            encode_table_row(&row_context, &mut heap, *row_id, row_version, cells)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let row_index = build_row_index(&mut heap, &row_ids, index_header)?;
    let rows_node = (!rows.is_empty())
        .then(|| NodeId::new(LTP_SUBNODE, 0x80))
        .transpose()?;
    let context = TableContextInfo::new(
        end_4b, end_4b, end_1b, end_bitmap, row_index, rows_node, columns,
    )?;
    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;
    heap.replace(user_root, context_bytes)?;
    let pages = heap.finish(HeapNodeType::Table, user_root)?;
    let rows_per_block = MAX_DATA_BLOCK_SIZE / usize::from(end_bitmap);
    let row_chunks = rows
        .chunks(rows_per_block)
        .map(|chunk| chunk.concat())
        .collect::<Vec<_>>();
    Ok((
        pages,
        rows_node.map(|node| (node, row_chunks, cell_subnodes)),
    ))
}

#[cfg(test)]
fn attachment_table_data(
    attachment_ids: &[NodeId],
    attachments: &[UnicodePstAttachment<'_>],
    sizes: &[i32],
    row_version: u32,
) -> io::Result<RebuiltRecipientTable> {
    attachment_table_data_keyed(attachment_ids, attachments, sizes, row_version)
        .map(|(table, _)| table)
}

/// `attachment_table_data`, plus each cell written in a subnode as (its node, the attachment's position, the column's
/// property ID), so that the cell can share the block of its attachment's value.
fn attachment_table_data_keyed(
    attachment_ids: &[NodeId],
    attachments: &[UnicodePstAttachment<'_>],
    sizes: &[i32],
    row_version: u32,
) -> io::Result<(RebuiltRecipientTable, Vec<(NodeId, usize, u16)>)> {
    let columns = vec![
        TableColumnDescriptor::new(PropertyType::Integer32, 0x0E20, 8, 4, 2),
        TableColumnDescriptor::new(PropertyType::Unicode, 0x3704, 12, 4, 3),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x3705, 16, 4, 4),
        TableColumnDescriptor::new(PropertyType::Integer32, 0x370B, 20, 4, 5),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_ID_PROP_ID, 0, 4, 0),
        TableColumnDescriptor::new(PropertyType::Integer32, LTP_ROW_VERSION_PROP_ID, 4, 4, 1),
    ];
    let row_context =
        TableContextInfo::new(24, 24, 24, 25, HeapId::default(), None, columns.clone())?;
    let mut heap = HeapBuilder::new();
    // The row index's header at HID 0x20, the TCINFO at 0x40.
    let index_header = reserve_bth_header(&mut heap)?;
    let user_root = reserve_tcinfo(&mut heap, &row_context)?;
    let (mut cell_subnodes, mut next_cell_subnode) = (Vec::new(), 0x81_u32);
    let mut cell_keys = Vec::new();
    let rows = attachment_ids
        .iter()
        .zip(attachments)
        .zip(sizes)
        .enumerate()
        .map(|(position, ((attachment_id, attachment), size))| {
            // The rows duplicate the attachment's properties: an attached message has method 5 and no
            // PidTagAttachFilename.
            let attached = attachment.attached_message.is_some();
            let mut cells = vec![
                Some(TableCell::Value(PropertyValue::Integer32(*size))),
                if attached {
                    None
                } else {
                    string_table_cell(PropertyType::Unicode, attachment.filename)
                },
                Some(TableCell::Value(PropertyValue::Integer32(if attached {
                    5
                } else {
                    1
                }))),
                Some(TableCell::Value(PropertyValue::Integer32(-1))),
                Some(TableCell::Value(PropertyValue::Integer32(
                    u32::from(*attachment_id) as i32,
                ))),
                Some(TableCell::Value(PropertyValue::Integer32(
                    row_version as i32,
                ))),
            ];
            // A file name too long for the heap goes to a subnode after the row matrix's (0x80).
            for (column, cell) in columns.iter().zip(cells.iter_mut()) {
                if let Some(cell) = cell {
                    spill_table_cell(
                        cell,
                        column.prop_type(),
                        &mut next_cell_subnode,
                        &mut cell_subnodes,
                    )?;
                    if let TableCell::Node(node) = cell {
                        cell_keys.push((*node, position, column.prop_id()));
                    }
                }
            }
            encode_table_row(&row_context, &mut heap, *attachment_id, row_version, cells)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let row_index = build_row_index(&mut heap, attachment_ids, index_header)?;
    let rows_node = (!rows.is_empty())
        .then(|| NodeId::new(LTP_SUBNODE, 0x80))
        .transpose()?;
    let context = TableContextInfo::new(24, 24, 24, 25, row_index, rows_node, columns)?;
    let mut context_bytes = Vec::new();
    context.write(&mut context_bytes)?;
    heap.replace(user_root, context_bytes)?;
    let pages = heap.finish(HeapNodeType::Table, user_root)?;
    let rows_per_block = MAX_DATA_BLOCK_SIZE / 25;
    let row_chunks = rows
        .chunks(rows_per_block)
        .map(|chunk| chunk.concat())
        .collect::<Vec<_>>();
    Ok((
        (
            pages,
            rows_node.map(|node| (node, row_chunks, cell_subnodes)),
        ),
        cell_keys,
    ))
}

fn validate_folder_path(folder_path: &[&str]) -> io::Result<()> {
    if folder_path.is_empty() || folder_path.iter().any(|name| name.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "folder path must contain only non-empty names",
        ));
    }
    // A new folder's property context is one heap page (`folder_data`), so its display name must fit one heap
    // allocation, 3,580 bytes (MS-PST 2.3.1): 1,790 UTF-16 code units. Exchange limits folder names to 250 characters
    // (MS-STANXIMAP C0004), far below.
    // ponytail: refused, not spilled to a subnode; spill it as attachment contexts do if a source gives longer names.
    if folder_path
        .iter()
        .any(|name| name.encode_utf16().count() * 2 > MAX_HN_ALLOCATION_SIZE)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder name is limited to 1,790 UTF-16 code units (one heap allocation, MS-PST 2.3.1)",
        ));
    }
    Ok(())
}

fn folder_data(name: &str, content_count: usize, has_subfolders: bool) -> io::Result<Vec<u8>> {
    let content_count = u32::try_from(content_count).map_err(|_| PstError::IntegerConversion)?;
    property_context_data(&[
        (0x3001, PropertyType::Unicode, Some(utf16_bytes(name)), 0),
        (0x3602, PropertyType::Integer32, None, content_count),
        (0x3603, PropertyType::Integer32, None, 0),
        (
            0x360A,
            PropertyType::Boolean,
            None,
            u32::from(has_subfolders),
        ),
        (
            0x3613,
            PropertyType::Unicode,
            Some(utf16_bytes("IPF.Note")),
            0,
        ),
    ])
}

/// `PidTagSearchKey` bytes: the caller's key, or the legacy derivation from the Message-ID. `check_message` refuses a
/// message that has neither.
fn search_key_bytes(input: &UnicodePstMessage<'_>) -> Vec<u8> {
    input
        .search_key
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| input.message_id.unwrap_or_default().as_bytes().to_vec())
}

/// The input rules of every message, an attached one included: a message class as MS-OXCMSG 2.2.1.3 allows it, and a
/// search key or a Message-ID to derive one from.
fn check_message(input: &UnicodePstMessage<'_>) -> io::Result<()> {
    let class = input.message_class;
    if class.is_empty()
        || class.len() > 255
        || class.ends_with('.')
        || !class.bytes().all(|byte| (0x20..=0x7F).contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a message class must be 1 to 255 ASCII characters from 0x20 to 0x7F and must not end with a period (MS-OXCMSG 2.2.1.3)",
        ));
    }
    if input.search_key.is_none() && input.message_id.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a message needs a search key or a Message-ID",
        ));
    }
    check_properties(input.properties, MESSAGE_OWNED_IDS, "message")?;
    check_named_properties(input.named_properties)?;
    let mut types = std::collections::BTreeMap::new();
    for recipient in input.recipients {
        check_properties(recipient.properties, RECIPIENT_OWNED_IDS, "recipient")?;
        for property in recipient.properties {
            if *types
                .entry(property.id)
                .or_insert(property.value.prop_type())
                != property.value.prop_type()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "recipient property {:#06x} has two types in one message",
                        property.id
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// `PidTagMessageFlags`: `MSGFLAG_READ` and `mfUnsent` from the input, `MSGFLAG_HASATTACH` from the attachments.
fn message_flags(input: &UnicodePstMessage<'_>, attachments: &[UnicodePstAttachment<'_>]) -> i32 {
    i32::from(input.is_read)
        | if input.unsent { 0x08 } else { 0 }
        | if attachments.is_empty() { 0 } else { 0x10 }
}

/// A message's property context. It holds no PidTagLtpRowId or PidTagLtpRowVer (0x67F2, 0x67F3). Those are the columns
/// of a table context's rows (MS-PST 2.3.4.4.1), which the contents row carries; Outlook-written stores keep them out
/// of every property context (0 of 89 PCs in two Outlook-written samples).
fn message_data(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    names: &NameIds,
) -> io::Result<RebuiltPropertyContext> {
    message_data_keyed(input, attachments, names).map(|(pc, _)| pc)
}

fn message_data_keyed(
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    names: &NameIds,
) -> io::Result<(RebuiltPropertyContext, Vec<(u16, NodeId)>)> {
    let has_attachments = !attachments.is_empty();
    let unicode =
        |value: &str| PropertyValue::Unicode(UnicodeValue::new(value.encode_utf16().collect()));
    check_message(input)?;
    // The sender (with its sent-representing mirror), the delivery time and the Message-ID are written only when given;
    // an absent one is PropertyValue::Null, which `property_context_pages` leaves out. The order of this list is the
    // order of the heap allocations, so it stays as it was: a message with all of them is unchanged, byte for byte.
    let sender = |value: for<'s> fn(&'s UnicodePstSender<'s>) -> &'s str| {
        input
            .sender
            .as_ref()
            .map_or(PropertyValue::Null, |sender| unicode(value(sender)))
    };
    let mut properties = vec![
        (
            0x0017,
            PropertyValue::Integer32(input.importance.importance()),
        ),
        (0x001A, unicode(input.message_class)),
        (0x0037, unicode(input.subject)),
        (
            0x0042,
            sender(|sender| display_name(sender.name, sender.email)),
        ),
        (0x0064, sender(|sender| sender.address_type)),
        (0x0065, sender(|sender| sender.email)),
        (0x0070, unicode(input.subject)),
        (
            0x0C1A,
            sender(|sender| display_name(sender.name, sender.email)),
        ),
        (0x0C1E, sender(|sender| sender.address_type)),
        (0x0C1F, sender(|sender| sender.email)),
        (
            0x0E06,
            input
                .delivery_time
                .map_or(PropertyValue::Null, PropertyValue::Time),
        ),
        (
            0x0E07,
            PropertyValue::Integer32(message_flags(input, attachments)),
        ),
        // PidTagMessageSize depends on these pages; the writer sets it with `set_inline_property`.
        (0x0E08, PropertyValue::Integer32(0)),
        (0x0E17, PropertyValue::Integer32(0)),
        (0x0E1B, PropertyValue::Boolean(has_attachments)),
        (0x1000, unicode(input.body)),
        (
            0x1035,
            input.message_id.map_or(PropertyValue::Null, unicode),
        ),
        (0x3007, PropertyValue::Time(input.creation_time)),
        (0x3008, PropertyValue::Time(input.last_modification_time)),
        (
            0x300B,
            PropertyValue::Binary(BinaryValue::new(search_key_bytes(input))),
        ),
        (0x3FDE, PropertyValue::Integer32(65001)),
    ];
    for (id, recipient_type) in [
        (0x0E02, UnicodePstRecipientType::Bcc),
        (0x0E03, UnicodePstRecipientType::Cc),
        (0x0E04, UnicodePstRecipientType::To),
    ] {
        let display = recipient_display(input.recipients, recipient_type);
        if !display.is_empty() {
            properties.push((id, unicode(&display)));
        }
    }
    if let Some(submit_time) = input.client_submit_time {
        properties.push((0x0039, PropertyValue::Time(submit_time)));
    }
    if let Some(headers) = input.transport_headers {
        properties.push((0x007D, unicode(headers)));
    }
    if let Some(html) = input.html_body {
        properties.push((
            0x1013,
            PropertyValue::Binary(BinaryValue::new(html.as_bytes().to_vec())),
        ));
    }
    if !input.categories.is_empty() {
        let tag = names.keywords.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "categories given but no Keywords property ID was resolved",
            )
        })?;
        properties.push((
            tag,
            PropertyValue::MultipleUnicode(
                input
                    .categories
                    .iter()
                    .map(|value| UnicodeValue::new(value.encode_utf16().collect()))
                    .collect(),
            ),
        ));
    }
    // The given properties, after the writer's own.
    properties.extend(
        input
            .properties
            .iter()
            .map(|property| (property.id, property.value.property_value())),
    );
    // The given named properties, under the IDs the store's map gives them.
    for property in input.named_properties {
        properties.push((names.given(property)?, property.value.property_value()));
    }
    property_context_pages_keyed(properties, &[], 0x80)
}

#[cfg(test)]
fn attachment_data(
    input: &UnicodePstAttachment<'_>,
    data_node: Option<NodeId>,
) -> io::Result<RebuiltPropertyContext> {
    attachment_data_keyed(input, data_node).map(|(pc, _)| pc)
}

fn attachment_data_keyed(
    input: &UnicodePstAttachment<'_>,
    data_node: Option<NodeId>,
) -> io::Result<(RebuiltPropertyContext, Vec<(u16, NodeId)>)> {
    let mut properties = vec![(0x0E20, PropertyValue::Integer32(attachment_size(input)?))];
    properties.extend(attachment_properties(input)?);
    // Built like a message's property context (MS-PST 2.3.3): a value over 3,580 bytes, such as a long file name, is a
    // subnode of the attachment node (2.3.3.3), and the heap takes as many pages as it needs (one heap page holding
    // every value would make a 1,900-character file name a 3,800-byte allocation). The data's own subnode (`data_node`,
    // LTP 0x200 + its position) and these values' (LTP from 0x80, at most six: the three file names, the extension, the
    // MIME type and the Content-ID) do not meet. PidTagAttachDataBinary is that subnode, or HNID 0 for empty content
    // (an empty value).
    match data_node {
        Some(node) => {
            property_context_pages_keyed(properties, &[(0x3701, PropertyType::Binary, node)], 0x80)
        }
        None => {
            properties.push((0x3701, PropertyValue::Binary(BinaryValue::new(Vec::new()))));
            property_context_pages_keyed(properties, &[], 0x80)
        }
    }
}

/// A file attachment's properties apart from PidTagAttachSize and PidTagAttachDataBinary: the ones `attachment_size`
/// counts, so the size and the stored values come from the same list.
fn attachment_properties(
    input: &UnicodePstAttachment<'_>,
) -> io::Result<Vec<(u16, PropertyValue)>> {
    let unicode =
        |value: &str| PropertyValue::Unicode(UnicodeValue::new(value.encode_utf16().collect()));
    // PidTagDisplayName and PidTagAttachExtension, as MS-OXCMAIL 2.2.3.4.2's table has a MIME reader store them;
    // Outlook for Mac names a file attachment from PidTagDisplayName.
    let mut properties = vec![
        (0x3001, unicode(input.filename)),
        (0x3704, unicode(input.filename)),
        (0x3705, PropertyValue::Integer32(1)),
        (0x3707, unicode(input.filename)),
        (0x370B, PropertyValue::Integer32(-1)),
        (0x370E, unicode(input.mime_type)),
        // PidTagAttachmentHidden: inline parts are not listed as attachments by mail clients.
        (0x7FFE, PropertyValue::Boolean(input.hidden)),
    ];
    if let Some(extension) = attachment_extension(input.filename) {
        properties.push((0x3703, unicode(&extension)));
    }
    if let Some(content_id) = attachment_content_id(input)? {
        properties.push((0x3712, unicode(content_id)));
        if input.hidden {
            // PidTagAttachFlags bit 0x4 (ATT_MHTML_REF) marks an inline attachment, one the HTML body renders
            // (MS-OXCMAIL 2.1.3.4.1.2). Set on a listed attachment with a Content-ID it can hide it (2.2.3.4.1.4.1).
            properties.push((0x3714, PropertyValue::Integer32(0x0000_0004)));
        }
    }
    Ok(properties)
}

fn attachment_content_id<'a>(input: &'a UnicodePstAttachment<'_>) -> io::Result<Option<&'a str>> {
    let Some(content_id) = input.content_id else {
        return Ok(None);
    };
    let content_id = content_id.strip_prefix('<').unwrap_or(content_id);
    let content_id = content_id.strip_suffix('>').unwrap_or(content_id);
    if content_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment Content-ID must not be empty",
        ));
    }
    Ok(Some(content_id))
}

/// PidTagAttachExtension of a file attachment: "." and the part of the file name after its last period, its Unicode
/// separators replaced with spaces and leading and trailing spaces and periods removed (MS-OXCMAIL 2.2.3.4.1.1, steps 1
/// to 3); none when that part is empty or the name has no period.
fn attachment_extension(filename: &str) -> Option<String> {
    let (_, extension) = filename.rsplit_once('.')?;
    // Unicode 5.1's separators (Zs, Zl, Zp), the version MS-OXCMAIL names; U+180E was Zs then.
    let separator = |c: char| {
        matches!(
            c,
            '\u{A0}' | '\u{1680}' | '\u{180E}' | '\u{2000}'
                ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
        )
    };
    let extension: String = extension
        .chars()
        .map(|c| if separator(c) { ' ' } else { c })
        .collect();
    let extension = extension.trim_matches([' ', '.']);
    (!extension.is_empty()).then(|| format!(".{extension}"))
}

/// PidTagAttachSize of a file attachment: the sum of its property values' sizes, PidTagAttachSize's own four bytes and
/// the data's length included, a fixed-size value at its type's size (PtypBoolean one byte, MS-OXCDATA 2.11.1) and a
/// string at its UTF-16 bytes. Outlook's own file attachments carry exactly this, and SCANPST repairs any other value
/// to it; a hand-kept formula that leaves out PidTagAttachmentHidden is one byte short on every file attachment.
fn attachment_size(input: &UnicodePstAttachment<'_>) -> io::Result<i32> {
    let mut size = attachment_len(input)?.checked_add(4);
    for (_, value) in attachment_properties(input)? {
        size = size
            .zip(stored_value_size(&value)?)
            .and_then(|(size, value)| size.checked_add(value));
    }
    let size = size.ok_or(PstError::IntegerConversion)?;
    // An attachment object larger than PidTagAttachSize can state is refused by name.
    i32::try_from(size).map_err(|_| too_large(size))
}

/// A property value's size as PidTagAttachSize counts it (`attachment_size`); only the types a file attachment carries.
fn stored_value_size(value: &PropertyValue) -> io::Result<Option<usize>> {
    match value {
        PropertyValue::Boolean(_) => Ok(Some(1)),
        PropertyValue::Integer32(_) => Ok(Some(4)),
        PropertyValue::Unicode(value) => Ok(value.buffer().len().checked_mul(2)),
        PropertyValue::Binary(value) => Ok(Some(value.buffer().len())),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no attachment size rule for {value:?}"),
        )),
    }
}

enum NewBlockData {
    Data(Vec<u8>),
    DataTree(Vec<UnicodeDataTreeEntry>, u8, u32),
    SubNodes(Vec<UnicodeLeafSubNodeTreeEntry>),
    IntermediateSubNodes(Vec<UnicodeIntermediateSubNodeTreeEntry>, u8),
}

struct NewBlock {
    id: UnicodeBlockId,
    offset: u64,
    size: u16,
    data: NewBlockData,
    /// The block's reference count when it is created: 1 (the reference counting adds its references), or its final
    /// count when its creator already knows it (the leaves and index blocks of a streamed data tree).
    ref_count: u16,
    /// An index block whose children's references its creator counted (see `Changes::precounted`).
    precounted: bool,
}

/// The block B-tree entry of a new block.
fn new_block_entry(block: &NewBlock) -> UnicodeBlockBTreeEntry {
    let mut entry = UnicodeBlockBTreeEntry::new(
        UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
        block.size,
    );
    if block.ref_count != 1 {
        entry.set_ref_count(block.ref_count);
    }
    entry
}

/// The reference page writer the stack encoder (`session::encode_btree_page`) is tested against.
#[cfg(test)]
enum NewBTreePageData {
    BlockLeaf(Vec<UnicodeBlockBTreeEntry>),
    NodeLeaf(Vec<UnicodeNodeBTreeEntry>),
    Intermediate(PageType, u8, Vec<UnicodeBTreePageEntry>),
}

#[cfg(test)]
struct NewBTreePage {
    page_ref: UnicodePageRef,
    data: NewBTreePageData,
}

/// A B-tree as read from disk: each level's pages in key order with their items, so a commit
/// can keep every page whose items did not change. Only tests read a whole tree.
#[cfg(test)]
struct BTreeLayout<E> {
    leaves: BTreePages<E>,
    /// Intermediate pages; `inner[0]` is level 1.
    inner: Vec<BTreePages<(u64, UnicodePageRef)>>,
}

#[cfg(test)]
type BTreePages<T> = Vec<(UnicodePageRef, Vec<T>)>;

/// Block `(offset, size)` and B-tree page offsets a commit already knows are allocated.
type KnownAllocations<'a> = (&'a [(u64, u16)], &'a [u64]);

#[cfg(test)]
enum BTreePageItems<E> {
    Leaf(Vec<E>),
    Intermediate(u8, Vec<(u64, UnicodePageRef)>),
}

#[cfg(test)]
impl<E: Copy + BTreeEntry<Key = u64>> BTreeLayout<E> {
    fn read(
        file: &mut File,
        root: UnicodePageRef,
        read_page: fn(&mut File, UnicodePageRef) -> io::Result<BTreePageItems<E>>,
    ) -> io::Result<Self> {
        let mut layout = Self {
            leaves: Vec::new(),
            inner: Vec::new(),
        };
        let mut stack = vec![(root, 0_u8)];
        while let Some((page_ref, depth)) = stack.pop() {
            if depth > 8 {
                return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
            }
            match read_page(file, page_ref)? {
                BTreePageItems::Leaf(entries) => layout.leaves.push((page_ref, entries)),
                BTreePageItems::Intermediate(level, children) => {
                    let level = usize::from(level.max(1));
                    if layout.inner.len() < level {
                        layout.inner.resize_with(level, Vec::new);
                    }
                    stack.extend(children.iter().rev().map(|(_, child)| (*child, depth + 1)));
                    layout.inner[level - 1].push((page_ref, children));
                }
            }
        }
        Ok(layout)
    }

    fn entries(&self) -> Vec<E> {
        let mut entries = self
            .leaves
            .iter()
            .flat_map(|(_, entries)| entries.iter().copied())
            .collect::<Vec<_>>();
        entries.sort_by_key(BTreeEntry::key);
        entries
    }
}

#[cfg(test)]
fn read_block_btree(
    file: &mut File,
    root: UnicodePageRef,
) -> io::Result<BTreeLayout<UnicodeBlockBTreeEntry>> {
    BTreeLayout::read(file, root, |file, page_ref| {
        Ok(match UnicodeBlockBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => BTreePageItems::Leaf(page.entries().to_vec()),
            RootBTreePage::Intermediate(page, ..) => BTreePageItems::Intermediate(
                page.level(),
                page.entries()
                    .iter()
                    .map(|entry| (entry.key(), entry.block()))
                    .collect(),
            ),
        })
    })
}

#[cfg(test)]
fn read_node_btree(
    file: &mut File,
    root: UnicodePageRef,
) -> io::Result<BTreeLayout<UnicodeNodeBTreeEntry>> {
    BTreeLayout::read(file, root, |file, page_ref| {
        Ok(match UnicodeNodeBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => BTreePageItems::Leaf(page.entries().to_vec()),
            RootBTreePage::Intermediate(page, ..) => BTreePageItems::Intermediate(
                page.level(),
                page.entries()
                    .iter()
                    .map(|entry| (entry.key(), entry.block()))
                    .collect(),
            ),
        })
    })
}

#[cfg(test)]
fn collect_block_btree_entries(
    file: &mut File,
    page_ref: UnicodePageRef,
) -> io::Result<Vec<UnicodeBlockBTreeEntry>> {
    Ok(read_block_btree(file, page_ref)?.entries())
}

#[cfg(test)]
fn collect_node_btree_entries(
    file: &mut File,
    page_ref: UnicodePageRef,
) -> io::Result<Vec<UnicodeNodeBTreeEntry>> {
    Ok(read_node_btree(file, page_ref)?.entries())
}

fn same_btree_entry<E: BTreeEntryReadWrite>(left: &E, right: &E) -> bool {
    let (mut a, mut b) = (Vec::with_capacity(32), Vec::with_capacity(32));
    left.write(&mut a).is_ok() && right.write(&mut b).is_ok() && a == b
}

#[cfg(test)]
fn same_page_ref(left: &UnicodePageRef, right: &UnicodePageRef) -> bool {
    left.block() == right.block() && left.index().index() == right.index().index()
}

#[cfg(test)]
fn write_btree_page(file: &mut File, page: NewBTreePage) -> io::Result<()> {
    let page_type = match &page.data {
        NewBTreePageData::BlockLeaf(_) => PageType::BlockBTree,
        NewBTreePageData::NodeLeaf(_) => PageType::NodeBTree,
        NewBTreePageData::Intermediate(page_type, ..) => *page_type,
    };
    let trailer = UnicodePageTrailer::new(
        page_type,
        page_type.signature(
            page.page_ref.index().index(),
            page.page_ref.block().into_u64(),
        ),
        page.page_ref.block(),
        0,
    );
    match page.data {
        NewBTreePageData::BlockLeaf(entries) => UnicodeBlockBTree::Leaf(Box::new(
            UnicodeBlockBTreePage::new(0, 20, 24, &entries, trailer)?,
        ))
        .write(file, page.page_ref),
        NewBTreePageData::NodeLeaf(entries) => UnicodeNodeBTree::Leaf(Box::new(
            UnicodeNodeBTreePage::new(0, 15, 32, &entries, trailer)?,
        ))
        .write(file, page.page_ref),
        NewBTreePageData::Intermediate(_, level, entries) => {
            file.seek(SeekFrom::Start(page.page_ref.index().index()))?;
            let page = UnicodeBTreeEntryPage::new(level, 20, 24, &entries, trailer)?;
            <UnicodeBTreeEntryPage as UnicodeBTreePageReadWrite<UnicodeBTreePageEntry>>::write(
                &page, file,
            )
        }
    }
}

fn push_block(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    internal: bool,
    data: NewBlockData,
) -> io::Result<UnicodeBlockId> {
    let id = UnicodeBlockId::new(internal, *next_bid)?;
    *next_bid += 1;
    push_block_with_id(blocks, next_offset, id, data)?;
    Ok(id)
}

fn push_block_with_id(
    blocks: &mut Vec<NewBlock>,
    next_offset: &mut FileSpace,
    id: UnicodeBlockId,
    data: NewBlockData,
) -> io::Result<()> {
    let size = match &data {
        NewBlockData::Data(data) => u16::try_from(data.len()),
        NewBlockData::DataTree(entries, ..) => u16::try_from(
            usize::from(DataTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeDataTreeEntry::ENTRY_SIZE),
        ),
        NewBlockData::SubNodes(entries) => u16::try_from(
            usize::from(UnicodeSubNodeTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeLeafSubNodeTreeEntry::ENTRY_SIZE),
        ),
        NewBlockData::IntermediateSubNodes(entries, ..) => u16::try_from(
            usize::from(UnicodeSubNodeTreeBlockHeader::HEADER_SIZE)
                + entries.len() * usize::from(UnicodeIntermediateSubNodeTreeEntry::ENTRY_SIZE),
        ),
    }
    .map_err(|_| PstError::IntegerConversion)?;
    UnicodeBlockTrailer::new(size, 0, 0, id)?;
    let disk_size = u64::from(ndb::block::block_size(size + UnicodeBlockTrailer::SIZE));
    let offset = reserve_file_range(next_offset, disk_size, 64);
    blocks.push(NewBlock {
        id,
        offset,
        size,
        data,
        ref_count: 1,
        precounted: false,
    });
    Ok(())
}

fn map_page_count(amap_index: u64) -> u64 {
    let has_pmap = amap_index % PMAP_PAGE_COUNT == 0;
    let has_fmap = has_pmap
        && amap_index >= FMAP_FIRST_SIZE
        && (amap_index - FMAP_FIRST_SIZE) % FMAP_PAGE_COUNT == 0;
    let has_fpmap = has_pmap
        && amap_index >= FPMAP_FIRST_SIZE
        && (amap_index - FPMAP_FIRST_SIZE) % FPMAP_PAGE_COUNT == 0;
    1 + u64::from(has_pmap) + u64::from(has_fmap) + u64::from(has_fpmap)
}

/// Where new blocks and pages go: space the committed AMap marks free, then EOF.
/// Free space is unreferenced by the committed B-trees, so overwriting it before
/// the header switches over keeps the old state intact.
struct FileSpace {
    eof: u64,
    free: Vec<(u64, u64)>,
    /// The largest run in `free`; a larger request goes straight to EOF without scanning.
    max_run: u64,
    /// Inside a writer session: what this commit allocates, so the session can update the allocation map without
    /// keeping per-block records. `None` outside a session.
    record: Option<Box<session::AllocRecord>>,
}

impl From<u64> for FileSpace {
    fn from(eof: u64) -> Self {
        Self {
            eof,
            free: Vec::new(),
            max_run: 0,
            record: None,
        }
    }
}

impl FileSpace {
    #[cfg(test)]
    fn read(file: &mut File, root: &UnicodeRoot) -> io::Result<Self> {
        let mut space = Self::from(root.file_eof_index().index());
        if root.amap_is_valid() == AmapStatus::Invalid {
            return Ok(space);
        }
        let mut page = [0; PAGE_SIZE];
        let mut amap = AMAP_FIRST_OFFSET;
        while amap < space.eof {
            file.seek(SeekFrom::Start(amap))?;
            file.read_exact(&mut page)?;
            let bits = size_of::<MapBits>();
            if page[bits] != PageType::AllocationMap as u8 {
                return Ok(Self::from(space.eof));
            }
            for bit in 0..bits * 8 {
                let offset = amap + bit as u64 * 64;
                if offset >= space.eof {
                    break;
                }
                if page[bit / 8] & (0x80 >> (bit % 8)) != 0 {
                    continue;
                }
                match space.free.last_mut() {
                    Some((_, end)) if *end == offset => *end += 64,
                    _ => space.free.push((offset, offset + 64)),
                }
            }
            amap += AMAP_DATA_SIZE;
        }
        space.max_run = space
            .free
            .iter()
            .map(|(start, end)| end - start)
            .max()
            .unwrap_or(0);
        Ok(space)
    }
}

fn reserve_file_range(space: &mut FileSpace, size: u64, alignment: u64) -> u64 {
    // ponytail: first-fit linear scan over a bounded list (a writer session keeps at most FREE_RUN_CAP runs); the
    // max_run guard skips it for requests no run can hold.
    if size <= space.max_run {
        for index in 0..space.free.len() {
            crate::measurement::count_op!(FREE_RUNS_SCANNED, 1);
            let (start, end) = space.free[index];
            let offset = start.div_ceil(alignment) * alignment;
            if offset + size <= end {
                let rest = [(start, offset), (offset + size, end)];
                space
                    .free
                    .splice(index..=index, rest.into_iter().filter(|(a, b)| a < b));
                if end - start == space.max_run {
                    space.max_run = space.free.iter().map(|(a, b)| b - a).max().unwrap_or(0);
                }
                if let Some(record) = space.record.as_mut() {
                    record.below(offset, size);
                }
                return offset;
            }
        }
    }
    let bump_start = space.eof;
    let offset = reserve_at_eof(&mut space.eof, size, alignment);
    if let Some(record) = space.record.as_mut() {
        record.bump(bump_start, offset, size);
    }
    offset
}

/// EOF allocation: the next `alignment`-aligned offset where `size` bytes fit inside one AMap range, after its map
/// pages.
fn reserve_at_eof(next_offset: &mut u64, size: u64, alignment: u64) -> u64 {
    let mut offset = next_offset.div_ceil(alignment) * alignment;
    loop {
        if offset < AMAP_FIRST_OFFSET {
            *next_offset = offset + size;
            return offset;
        }

        let amap_index = (offset - AMAP_FIRST_OFFSET) / AMAP_DATA_SIZE;
        let amap_offset = AMAP_FIRST_OFFSET + amap_index * AMAP_DATA_SIZE;
        let reserved_end = amap_offset + map_page_count(amap_index) * PAGE_SIZE as u64;
        if offset < reserved_end {
            offset = reserved_end.div_ceil(alignment) * alignment;
            continue;
        }

        let next_amap = amap_offset + AMAP_DATA_SIZE;
        if offset + size > next_amap {
            let next_index = amap_index + 1;
            offset = (next_amap + map_page_count(next_index) * PAGE_SIZE as u64)
                .div_ceil(alignment)
                * alignment;
            continue;
        }

        *next_offset = offset + size;
        return offset;
    }
}

fn push_data_tree(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    data: &[u8],
) -> io::Result<UnicodeBlockId> {
    push_data_tree_chunks(
        blocks,
        next_bid,
        next_offset,
        data.chunks(MAX_DATA_BLOCK_SIZE).map(<[u8]>::to_vec),
    )
}

fn push_data_tree_chunks(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    chunks: impl IntoIterator<Item = Vec<u8>>,
) -> io::Result<UnicodeBlockId> {
    let mut chunks = chunks.into_iter().collect::<Vec<_>>();
    if chunks.is_empty() || chunks.iter().any(Vec::is_empty) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data tree cannot contain empty blocks",
        ));
    }
    if chunks.iter().any(|chunk| chunk.len() > MAX_DATA_BLOCK_SIZE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data tree block exceeds the Unicode PST block limit",
        ));
    }
    let intermediate_count = chunks.len().saturating_sub(1);
    for chunk in chunks.iter_mut().take(intermediate_count) {
        chunk.resize(MAX_DATA_BLOCK_SIZE, 0);
    }
    let ids = chunks
        .into_iter()
        .map(|chunk| {
            let size = u32::try_from(chunk.len()).map_err(|_| PstError::IntegerConversion)?;
            let id = push_block(
                blocks,
                next_bid,
                next_offset,
                false,
                NewBlockData::Data(chunk),
            )?;
            Ok((id, size))
        })
        .collect::<io::Result<Vec<_>>>()?;
    push_data_tree_leaves(blocks, next_bid, next_offset, ids)
}

/// Build the XBLOCK levels over existing or new leaf data blocks, given as `(block, size)`.
fn push_data_tree_leaves(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    mut ids: Vec<(UnicodeBlockId, u32)>,
) -> io::Result<UnicodeBlockId> {
    if ids.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data tree cannot be empty",
        ));
    }
    let mut level = 1;
    while ids.len() > 1 {
        ids = ids
            .chunks(1021)
            .map(|entries| {
                let total_size = entries.iter().try_fold(0_u32, |total, (_, size)| {
                    total.checked_add(*size).ok_or(PstError::IntegerConversion)
                })?;
                let id = push_block(
                    blocks,
                    next_bid,
                    next_offset,
                    true,
                    NewBlockData::DataTree(
                        entries
                            .iter()
                            .map(|(id, _)| UnicodeDataTreeEntry::from(*id))
                            .collect(),
                        level,
                        total_size,
                    ),
                )?;
                Ok((id, total_size))
            })
            .collect::<io::Result<Vec<_>>>()?;
        level += 1;
    }
    Ok(ids[0].0)
}

fn push_recipient_table(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    table: RebuiltRecipientTable,
) -> io::Result<(UnicodeBlockId, Option<UnicodeBlockId>)> {
    push_table_sharing(blocks, next_bid, next_offset, table, |_, _| None)
}

/// A table's blocks, as `push_recipient_table` writes them, except that a cell in a subnode for which `shared` gives a
/// block is that block, not a copy.
fn push_table_sharing(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    table: RebuiltRecipientTable,
    shared: impl Fn(NodeId, &[u8]) -> Option<UnicodeBlockId>,
) -> io::Result<(UnicodeBlockId, Option<UnicodeBlockId>)> {
    let (pages, rows) = table;
    let data = push_data_tree_chunks(blocks, next_bid, next_offset, pages)?;
    let subnodes = if let Some((rows_node, row_chunks, cells)) = rows {
        let rows = push_data_tree_chunks(blocks, next_bid, next_offset, row_chunks)?;
        let mut entries = vec![UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None)];
        for (node, data) in cells {
            let data = match shared(node, &data) {
                Some(block) => block,
                None => push_data_tree(blocks, next_bid, next_offset, &data)?,
            };
            entries.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
        }
        Some(push_subnode_tree(blocks, next_bid, next_offset, entries)?)
    } else {
        None
    };
    Ok((data, subnodes))
}

fn push_subnode_tree(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    mut entries: Vec<UnicodeLeafSubNodeTreeEntry>,
) -> io::Result<UnicodeBlockId> {
    const LEAF_CAPACITY: usize = (MAX_DATA_BLOCK_SIZE
        - UnicodeSubNodeTreeBlockHeader::HEADER_SIZE as usize)
        / UnicodeLeafSubNodeTreeEntry::ENTRY_SIZE as usize;
    const INTERMEDIATE_CAPACITY: usize = (MAX_DATA_BLOCK_SIZE
        - UnicodeSubNodeTreeBlockHeader::HEADER_SIZE as usize)
        / UnicodeIntermediateSubNodeTreeEntry::ENTRY_SIZE as usize;

    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "subnode tree cannot be empty",
        ));
    }
    entries.sort_by_key(|entry| u32::from(entry.node()));
    let mut level_entries = entries
        .chunks(LEAF_CAPACITY)
        .map(|chunk| {
            let first = chunk[0].node();
            let block = push_block(
                blocks,
                next_bid,
                next_offset,
                true,
                NewBlockData::SubNodes(chunk.to_vec()),
            )?;
            Ok((first, block))
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut level = 1;
    while level_entries.len() > 1 {
        level_entries = level_entries
            .chunks(INTERMEDIATE_CAPACITY)
            .map(|chunk| {
                let first = chunk[0].0;
                let entries = chunk
                    .iter()
                    .map(|(node, block)| UnicodeIntermediateSubNodeTreeEntry::new(*node, *block))
                    .collect();
                let block = push_block(
                    blocks,
                    next_bid,
                    next_offset,
                    true,
                    NewBlockData::IntermediateSubNodes(entries, level),
                )?;
                Ok((first, block))
            })
            .collect::<io::Result<Vec<_>>>()?;
        level += 1;
    }
    Ok(level_entries[0].1)
}

/// Encodes blocks in memory and sends each run of contiguous blocks to the file in one write.
/// The block encoders write field by field, and on Windows every one of those small writes is a
/// costly system call.
struct BlockWriter {
    base: u64,
    position: u64,
    buffer: Vec<u8>,
    /// How data blocks are encoded: the file's bCryptMethod.
    crypt: NdbCryptMethod,
}

thread_local! {
    /// The run buffer of the last block writer, kept for the next one: a writer per commit would grow a new buffer by
    /// doubling, and the freed ones would leave the allocator's medium regions dirty commit after commit. Runs end at
    /// map pages, so the buffer stays near one allocation-map range (about 248 KiB); one over KEPT_RUN_BUFFER is not
    /// kept.
    static SPARE_RUN_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl BlockWriter {
    const MAX_RUN: usize = 8 << 20;
    const KEPT_RUN_BUFFER: usize = 1 << 20;

    fn new(crypt: NdbCryptMethod) -> Self {
        let buffer = SPARE_RUN_BUFFER.with(|spare| std::mem::take(&mut *spare.borrow_mut()));
        Self {
            base: 0,
            position: 0,
            buffer,
            crypt,
        }
    }

    fn push(&mut self, file: &mut File, block: NewBlock) -> io::Result<()> {
        let end = self.base + self.buffer.len() as u64;
        if !self.buffer.is_empty() && (block.offset != end || self.buffer.len() >= Self::MAX_RUN) {
            self.flush(file)?;
        }
        if self.buffer.is_empty() {
            self.base = block.offset;
        }
        crate::measurement::count_op!(BLOCKS_WRITTEN, 1);
        crate::measurement::count_op!(BLOCK_BYTES_WRITTEN, block.size);
        let crypt = self.crypt;
        write_new_block(self, block, crypt)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        if !self.buffer.is_empty() {
            file.seek(SeekFrom::Start(self.base))?;
            file.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        Ok(())
    }
}

impl Drop for BlockWriter {
    fn drop(&mut self) {
        let mut buffer = std::mem::take(&mut self.buffer);
        if buffer.capacity() <= Self::KEPT_RUN_BUFFER {
            buffer.clear();
            SPARE_RUN_BUFFER.with(|spare| *spare.borrow_mut() = buffer);
        }
    }
}

impl Write for BlockWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let start =
            usize::try_from(self.position - self.base).map_err(|_| PstError::IntegerConversion)?;
        let end = start + data.len();
        if self.buffer.len() < end {
            self.buffer.resize(end, 0);
        }
        self.buffer[start..end].copy_from_slice(data);
        self.position += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for BlockWriter {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => {
                (self.base + self.buffer.len() as u64).checked_add_signed(delta)
            }
        };
        match target {
            Some(offset) if offset >= self.base => {
                self.position = offset;
                Ok(offset)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block writes only seek within the current run",
            )),
        }
    }
}

thread_local! {
    /// Data-block buffers of written blocks, reused by the streaming append for its next blocks instead of one new
    /// allocation per 8 KiB block. Never more than one flush worth of buffers (`STREAMED_BLOCKS_PER_FLUSH`).
    // ponytail: thread-local pool; pass it explicitly if the writer ever writes from more than one thread.
    static SPARE_BLOCK_BUFFERS: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A buffer of `len` zero bytes for a streamed data block, from the spare buffers when there is one.
fn block_buffer(len: usize) -> Vec<u8> {
    let mut buffer = SPARE_BLOCK_BUFFERS
        .with(|spare| spare.borrow_mut().pop())
        .unwrap_or_default();
    buffer.clear();
    buffer.resize(len, 0);
    buffer
}

fn recycle_block_buffer(buffer: Vec<u8>) {
    // Only buffers of exactly one data block: the pool holds at most STREAMED_BLOCKS_PER_FLUSH * 8,176 bytes.
    if buffer.capacity() == MAX_DATA_BLOCK_SIZE {
        SPARE_BLOCK_BUFFERS.with(|spare| {
            let mut spare = spare.borrow_mut();
            if spare.len() < STREAMED_BLOCKS_PER_FLUSH {
                spare.push(buffer);
            }
        });
    }
}

fn write_new_block<W: Write + Seek>(
    file: &mut W,
    block: NewBlock,
    crypt: NdbCryptMethod,
) -> io::Result<()> {
    let block_signature = block_sig::compute_sig(block.offset as u32, u64::from(block.id) as u32);
    let trailer = UnicodeBlockTrailer::new(block.size, block_signature, 0, block.id)?;
    file.seek(SeekFrom::Start(block.offset))?;
    match block.data {
        NewBlockData::Data(mut data) => {
            // The block owns its buffer, so a permute-encoded file encodes it in place and writes it as it is. Encoding
            // a copy would cost one 8 KiB allocation per block, and the churn of those raises the peak RSS with the
            // attachment's size. The CRC covers the encoded bytes either way.
            let encoding = match crypt {
                NdbCryptMethod::Permute => {
                    crate::encode::permute::encode_block(&mut data);
                    NdbCryptMethod::None
                }
                other => other,
            };
            let block = UnicodeDataBlock::new(encoding, data, trailer)?;
            block.write(file)?;
            recycle_block_buffer(Vec::from(block));
            Ok(())
        }
        NewBlockData::DataTree(entries, level, total_size) => {
            let header = DataTreeBlockHeader::new(
                level,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
                total_size,
            );
            UnicodeDataTreeBlock::new(header, entries, trailer)?.write(file)
        }
        NewBlockData::SubNodes(entries) => {
            let header = UnicodeSubNodeTreeBlockHeader::new(
                0,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
            );
            let tree = UnicodeLeafSubNodeTreeBlock::new(header, entries, trailer)?;
            SubNodeTree::<UnicodePstFile>::Leaf(Box::new(tree)).write(
                file,
                &UnicodeBlockBTreeEntry::new(
                    UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                    block.size,
                ),
            )
        }
        NewBlockData::IntermediateSubNodes(entries, level) => {
            let header = UnicodeSubNodeTreeBlockHeader::new(
                level,
                u16::try_from(entries.len()).map_err(|_| PstError::IntegerConversion)?,
            );
            let tree = UnicodeIntermediateSubNodeTreeBlock::new(header, entries, trailer)?;
            SubNodeTree::<UnicodePstFile>::Intermediate(Box::new(tree)).write(
                file,
                &UnicodeBlockBTreeEntry::new(
                    UnicodeBlockRef::new(block.id, UnicodeByteIndex::new(block.offset)),
                    block.size,
                ),
            )
        }
    }
}

/// Builds a data tree from leaves given one at a time: an index block is written every 1,021 leaves, so neither the
/// leaves nor their IDs are held for the whole value. The tree is the one `push_data_tree_leaves` builds (1,021 leaves
/// per XBLOCK, an XXBLOCK over the XBLOCKs), only the index blocks get their IDs as they are written. Each leaf and
/// index block below the top is created with its final reference count (2: the B-tree and its one parent), and every
/// index block is marked precounted, so the commit's reference counting neither walks nor holds its children.
struct DataTreeBuilder {
    /// The number of leaves the value has (known from its length).
    expected: usize,
    leaves: Vec<(UnicodeBlockId, u32)>,
    index_blocks: Vec<(UnicodeBlockId, u32)>,
}

impl DataTreeBuilder {
    fn new(len: u64) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "data tree cannot contain empty blocks",
            ));
        }
        let expected = usize::try_from(len.div_ceil(MAX_DATA_BLOCK_SIZE as u64))
            .map_err(|_| PstError::IntegerConversion)?;
        if expected > 1021 * 1021 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the value exceeds the largest MS-PST data tree",
            ));
        }
        Ok(Self {
            expected,
            leaves: Vec::new(),
            index_blocks: Vec::new(),
        })
    }

    fn leaf(
        &mut self,
        blocks: &mut Vec<NewBlock>,
        next_bid: &mut u64,
        next_offset: &mut FileSpace,
        chunk: Vec<u8>,
    ) -> io::Result<()> {
        let size = u32::try_from(chunk.len()).map_err(|_| PstError::IntegerConversion)?;
        let id = push_block(
            blocks,
            next_bid,
            next_offset,
            false,
            NewBlockData::Data(chunk),
        )?;
        if self.expected > 1 {
            blocks.last_mut().expect("pushed").ref_count = 2;
        }
        self.leaves.push((id, size));
        if self.expected > 1021 && self.leaves.len() == 1021 {
            self.close_index(blocks, next_bid, next_offset, 2)?;
        }
        Ok(())
    }

    /// Write the index block over the leaves gathered so far, with reference count `refs`.
    fn close_index(
        &mut self,
        blocks: &mut Vec<NewBlock>,
        next_bid: &mut u64,
        next_offset: &mut FileSpace,
        refs: u16,
    ) -> io::Result<()> {
        let size = self.leaves.iter().try_fold(0_u32, |sum, (_, size)| {
            sum.checked_add(*size).ok_or(PstError::IntegerConversion)
        })?;
        let entries = self
            .leaves
            .drain(..)
            .map(|(id, _)| UnicodeDataTreeEntry::from(id))
            .collect();
        let id = push_block(
            blocks,
            next_bid,
            next_offset,
            true,
            NewBlockData::DataTree(entries, 1, size),
        )?;
        let block = blocks.last_mut().expect("pushed");
        block.ref_count = refs;
        block.precounted = true;
        self.index_blocks.push((id, size));
        Ok(())
    }

    fn finish(
        mut self,
        blocks: &mut Vec<NewBlock>,
        next_bid: &mut u64,
        next_offset: &mut FileSpace,
    ) -> io::Result<UnicodeBlockId> {
        if self.index_blocks.is_empty() && self.leaves.len() == 1 && self.expected == 1 {
            return Ok(self.leaves[0].0);
        }
        if self.expected <= 1021 {
            self.close_index(blocks, next_bid, next_offset, 1)?;
            return Ok(self.index_blocks[0].0);
        }
        if !self.leaves.is_empty() {
            self.close_index(blocks, next_bid, next_offset, 2)?;
        }
        let size = self.index_blocks.iter().try_fold(0_u32, |sum, (_, size)| {
            sum.checked_add(*size).ok_or(PstError::IntegerConversion)
        })?;
        let entries = self
            .index_blocks
            .iter()
            .map(|(id, _)| UnicodeDataTreeEntry::from(*id))
            .collect();
        let id = push_block(
            blocks,
            next_bid,
            next_offset,
            true,
            NewBlockData::DataTree(entries, 2, size),
        )?;
        blocks.last_mut().expect("pushed").precounted = true;
        Ok(id)
    }
}

/// Push the leaf data blocks of `data` one at a time and hand the pending blocks to `flush` every
/// `STREAMED_BLOCKS_PER_FLUSH` blocks, so a large value is never held a second time in memory as block copies. The data
/// tree has the shape `push_data_tree` builds.
const STREAMED_BLOCKS_PER_FLUSH: usize = 128;

fn push_data_tree_streamed(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    data: &[u8],
    flush: &mut dyn FnMut(&mut Vec<NewBlock>, &mut FileSpace) -> io::Result<()>,
) -> io::Result<UnicodeBlockId> {
    let mut tree = DataTreeBuilder::new(data.len() as u64)?;
    for chunk in data.chunks(MAX_DATA_BLOCK_SIZE) {
        // Every chunk but the last is exactly MAX_DATA_BLOCK_SIZE, so no padding is needed.
        let mut buffer = block_buffer(0);
        buffer.extend_from_slice(chunk);
        tree.leaf(blocks, next_bid, next_offset, buffer)?;
        if blocks.len() >= STREAMED_BLOCKS_PER_FLUSH {
            flush(blocks, next_offset)?;
        }
    }
    tree.finish(blocks, next_bid, next_offset)
}

/// `push_data_tree_streamed` for streamed content: reads the declared length in data blocks from a stream opened here,
/// then requires the end of the stream. The blocks and the tree's shape are the same as for the same bytes given as a
/// slice.
fn push_data_tree_from_stream(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    stream: UnicodePstAttachmentStream<'_>,
    name: &str,
    flush: &mut dyn FnMut(&mut Vec<NewBlock>, &mut FileSpace) -> io::Result<()>,
) -> io::Result<UnicodeBlockId> {
    let len = stream.len;
    let mut tree = DataTreeBuilder::new(len)?;
    let mut reader = (stream.open)()?;
    let mut done = 0_u64;
    while done < len {
        let want = usize::try_from((len - done).min(MAX_DATA_BLOCK_SIZE as u64))
            .map_err(|_| PstError::IntegerConversion)?;
        let mut chunk = block_buffer(want);
        let mut filled = 0;
        while filled < want {
            match reader.read(&mut chunk[filled..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "attachment {name} ended after {} of its declared {len} bytes",
                            done + filled as u64
                        ),
                    ))
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        done += want as u64;
        tree.leaf(blocks, next_bid, next_offset, chunk)?;
        if blocks.len() >= STREAMED_BLOCKS_PER_FLUSH {
            flush(blocks, next_offset)?;
        }
    }
    // The stream must end here: a source with more bytes than declared is never written cut off.
    let mut probe = [0_u8; 1];
    loop {
        match reader.read(&mut probe) {
            Ok(0) => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("attachment {name} has more bytes than its declared {len}"),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    tree.finish(blocks, next_bid, next_offset)
}

fn push_attachments(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    attachments: &[UnicodePstAttachment<'_>],
    table: RebuiltRecipientTable,
    table_cells: Vec<(NodeId, usize, u16)>,
    built: Vec<BuiltAttachment>,
    sub_nodes: &mut Vec<UnicodeLeafSubNodeTreeEntry>,
    flush: &mut dyn FnMut(&mut Vec<NewBlock>, &mut FileSpace) -> io::Result<()>,
) -> io::Result<()> {
    if attachments.is_empty() {
        return Ok(());
    }
    // The attachments first, so a table cell in a subnode can be the block of its attachment's value.
    let mut shared: Vec<SharedValues> = Vec::with_capacity(built.len());
    for (
        BuiltAttachment {
            node: attachment_id,
            content_node,
            pc: (attachment_pages, attachment_external),
            pc_ids,
            message,
        },
        attachment,
    ) in built.into_iter().zip(attachments)
    {
        // The attachment node's subnode tree: its content (an attached message or a file's data) and the property
        // context's values over 3,580 bytes.
        let mut attachment_subnodes = Vec::new();
        if let (Some(message), Some(node), Some(attached)) = (
            message,
            content_node,
            attachment.attached_message.as_deref(),
        ) {
            // The attached message, written like a top-level message, as the attachment node's subnode.
            let (message_bid, message_subnodes, _) = push_message(
                blocks,
                next_bid,
                next_offset,
                *message,
                &attached.attachments,
                flush,
            )?;
            attachment_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(
                node,
                message_bid,
                Some(message_subnodes),
            ));
        } else if let Some(data_node) = content_node {
            let data_block = match attachment.stream {
                Some(stream) => push_data_tree_from_stream(
                    blocks,
                    next_bid,
                    next_offset,
                    stream,
                    attachment.filename,
                    flush,
                )?,
                None => {
                    push_data_tree_streamed(blocks, next_bid, next_offset, attachment.data, flush)?
                }
            };
            attachment_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(
                data_node, data_block, None,
            ));
        }
        let mut values = SharedValues::new();
        for (node, data) in attachment_external {
            let block = push_data_tree(blocks, next_bid, next_offset, &data)?;
            if let Some((id, _)) = pc_ids.iter().find(|(_, value_node)| *value_node == node) {
                values.push((*id, block, data.len(), value_digest(&data)));
            }
            attachment_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, block, None));
        }
        shared.push(values);
        let attachment_subnode = if attachment_subnodes.is_empty() {
            None
        } else {
            Some(push_subnode_tree(
                blocks,
                next_bid,
                next_offset,
                attachment_subnodes,
            )?)
        };
        let attachment_block =
            push_data_tree_chunks(blocks, next_bid, next_offset, attachment_pages)?;
        sub_nodes.push(UnicodeLeafSubNodeTreeEntry::new(
            attachment_id,
            attachment_block,
            attachment_subnode,
        ));
    }
    // The same layout as the recipient table: heap pages, the row matrix and any large cells in its subnode tree.
    let (table_block, table_subnodes) =
        push_table_sharing(blocks, next_bid, next_offset, table, |node, data| {
            let (_, position, prop_id) = table_cells.iter().find(|(cell, _, _)| *cell == node)?;
            shared_block(shared.get(*position)?, *prop_id, data)
        })?;
    sub_nodes.push(UnicodeLeafSubNodeTreeEntry::new(
        NID_ATTACHMENT_TABLE,
        table_block,
        table_subnodes,
    ));
    Ok(())
}

/// Write a built message's blocks: its property context, and one subnode tree with its recipient table, its external
/// values and its attachments, each attached message the same way. Returns the property context's block and the subnode
/// tree's block.
fn push_message(
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    message: BuiltMessage,
    attachments: &[UnicodePstAttachment<'_>],
    flush: &mut dyn FnMut(&mut Vec<NewBlock>, &mut FileSpace) -> io::Result<()>,
) -> io::Result<(UnicodeBlockId, UnicodeBlockId, SharedValues)> {
    let BuiltMessage {
        pages,
        external,
        external_ids,
        parts,
        ..
    } = message;
    let message_bid = push_data_tree_chunks(blocks, next_bid, next_offset, pages)?;
    let (recipient_bid, recipient_subnodes) =
        push_recipient_table(blocks, next_bid, next_offset, parts.recipients)?;
    let mut message_subnodes = vec![UnicodeLeafSubNodeTreeEntry::new(
        NID_RECIPIENT_TABLE,
        recipient_bid,
        recipient_subnodes,
    )];
    // The values a contents row may share, by property ID.
    let mut shared = SharedValues::new();
    for (node, data) in external {
        let block = push_data_tree(blocks, next_bid, next_offset, &data)?;
        if let Some((id, _)) = external_ids
            .iter()
            .find(|(_, value_node)| *value_node == node)
        {
            shared.push((*id, block, data.len(), value_digest(&data)));
        }
        message_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, block, None));
    }
    push_attachments(
        blocks,
        next_bid,
        next_offset,
        attachments,
        parts.attachment_table,
        parts.attachment_cells,
        parts.attachments,
        &mut message_subnodes,
        flush,
    )?;
    let message_subnode_bid = push_subnode_tree(blocks, next_bid, next_offset, message_subnodes)?;
    Ok((message_bid, message_subnode_bid, shared))
}

fn create_unicode_pst(
    path: &Path,
    input: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    folder_path: Option<&[&str]>,
) -> io::Result<()> {
    if let Some(folder_path) = folder_path {
        validate_folder_path(folder_path)?;
    }
    // The template is set up; folders and the first message go through one writer session.
    let mut core = create_unicode_session(path)?;
    let inbox = ["Inbox"];
    let result = append_unicode_pst(
        &mut core,
        &[UnicodePstBatchMessage {
            message: *input,
            attachments,
        }],
        Some(folder_path.unwrap_or(&inbox)),
    );
    drop(core);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// A new store at `path`, the bundled template with its Inbox, as an open writer session (public as
/// `UnicodePstWriter::create`). The file must not exist; a failure removes it.
fn create_unicode_session(path: &Path) -> io::Result<WriterCore> {
    create_template_store(path)?;
    let result = (|| {
        let mut core = WriterCore::open(path)?;
        ensure_unicode_folder_path(&mut core, &["Inbox"])?;
        Ok(core)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// A new file at `path` holding the bundled template store as it is. Removed again on failure.
fn create_template_store(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    // The template stays as Outlook made it, permute-encoded (bCryptMethod 1, Outlook's default; two Outlook-written
    // samples use it). The writer encodes its own data blocks the same way.
    let setup = (|| {
        file.write_all(OUTLOOK_UNICODE_PST_TEMPLATE)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = setup {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}
fn default_receive_folder(
    store: &Rc<UnicodeStore>,
    nbt_entries: &[UnicodeNodeBTreeEntry],
) -> io::Result<NodeId> {
    let ipm = store.open_folder(&store.properties().ipm_sub_tree_entry_id()?)?;
    let hierarchy = ipm
        .hierarchy_table()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "IPM hierarchy table not found"))?;
    for row in hierarchy.rows_matrix() {
        let node = NodeId::from(u32::from(row.id()));
        let folder = store.open_folder(&store.properties().make_entry_id(node)?)?;
        // The exact name, as `create_unicode_pst` and `child_folder_names` use it. An ASCII-case-insensitive match
        // would pick a different folder (for example a user folder "inbox") whenever the first match is not the receive
        // folder.
        if folder
            .properties()
            .display_name()
            .is_ok_and(|name| name == "Inbox")
        {
            return Ok(node);
        }
    }

    let mut best = None;
    for entry in nbt_entries
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::ReceiveFolderTable)))
    {
        let Ok(table) = UnicodeTableContext::read(store.clone(), *entry) else {
            continue;
        };
        for row in table.rows_matrix() {
            let values = row.columns(table.context())?;
            let mut message_class = None;
            let mut folder = None;
            for (column, value) in table.context().columns().iter().zip(values) {
                let Some(value) = value else { continue };
                if matches!(&value, TableRowColumnValue::Heap(id) if u32::from(*id) == 0)
                    || matches!(&value, TableRowColumnValue::Node(id) if u32::from(*id) == 0)
                {
                    continue;
                }
                match column.prop_id() {
                    0x001A => match table.read_column(&value, column.prop_type())? {
                        PropertyValue::String8(value) => message_class = Some(value.to_string()),
                        PropertyValue::Unicode(value) => message_class = Some(value.to_string()),
                        _ => {}
                    },
                    0x6605 => {
                        if let PropertyValue::Integer32(value) =
                            table.read_column(&value, column.prop_type())?
                        {
                            if value != 0 {
                                folder = Some(NodeId::from(value as u32));
                            }
                        }
                    }
                    0x0FFF => {
                        if let PropertyValue::Binary(value) =
                            table.read_column(&value, column.prop_type())?
                        {
                            folder = Some(EntryId::try_from(value.buffer())?.node_id())
                        }
                    }
                    _ => {}
                }
            }
            let Some(folder) = folder else { continue };
            let class = message_class.unwrap_or_default();
            let rank = if class.eq_ignore_ascii_case("IPM.Note") {
                3
            } else if class.eq_ignore_ascii_case("IPM") {
                2
            } else if class.is_empty() {
                1
            } else {
                0
            };
            if rank > best.map(|(rank, _)| rank).unwrap_or(0) {
                best = Some((rank, folder));
            }
        }
    }
    if let Some((_, folder)) = best {
        // Fallback: the store's own receive-folder table. Its folder is used only inside the IPM subtree. For the
        // bundled template (an Outlook-created store without an Inbox) it names the root folder 0x122, outside the IPM
        // subtree, where Outlook shows no mail: a folderless append would file the message out of sight. It is refused,
        // with the reason, and the caller names a folder.
        let ipm_node = store.properties().ipm_sub_tree_entry_id()?.node_id();
        if in_ipm_subtree(store, ipm_node, folder)? {
            return Ok(folder);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the store's default receive folder {:#x} is outside the IPM subtree, where Outlook shows no mail; name a folder",
                u32::from(folder)
            ),
        ));
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "default receive-folder mapping not found",
    ))
}

/// Whether `target` is the IPM subtree's root folder or a folder below it, by walking the hierarchy tables down from
/// the root.
// ponytail: walks every folder of the subtree; only a folderless append to a store without an exact "Inbox" gets here.
fn in_ipm_subtree(store: &Rc<UnicodeStore>, ipm: NodeId, target: NodeId) -> io::Result<bool> {
    let mut pending = vec![ipm];
    let mut seen = HashSet::new();
    while let Some(node) = pending.pop() {
        if u32::from(node) == u32::from(target) {
            return Ok(true);
        }
        if !seen.insert(u32::from(node)) {
            continue;
        }
        let folder = store.open_folder(&store.properties().make_entry_id(node)?)?;
        if let Some(hierarchy) = folder.hierarchy_table() {
            pending.extend(
                hierarchy
                    .rows_matrix()
                    .map(|row| NodeId::from(u32::from(row.id()))),
            );
        }
    }
    Ok(false)
}

/// The property ID of PidNameKeywords (PS_PUBLIC_STRINGS, string name "Keywords", MS-OXPROPS) in this store: 0x8000 plus
/// the property index (wPropIdx) of the matching string-named entry of the name-to-id map (MS-PST 2.4.7). This lookup adds
/// no entry (`NameMap` adds them, for given named properties); a store without it is refused.
fn keywords_property_id(store: &Rc<UnicodeStore>) -> io::Result<u16> {
    let map = UnicodeNamedPropertyMap::read(store.clone())
        .map_err(|err| io::Error::new(err.kind(), format!("read named-property map: {err}")))?;
    let properties = map.properties();
    let wanted: Vec<u8> = "Keywords"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    // A string offset beyond the stream would make lookup_string slice out of bounds; such an entry cannot name
    // "Keywords" and is skipped (the reader side reports a malformed map).
    let string_stream_len = match properties.get(0x0004) {
        Some(PropertyValue::Binary(value)) => value.buffer().len(),
        _ => 0,
    };
    for entry in properties.stream_entry()? {
        if let (NamedPropertyGuid::PublicStrings, NamedPropertyId::StringOffset(offset)) =
            (entry.guid(), entry.id())
        {
            if usize::try_from(offset)
                .map_or(true, |offset| offset.saturating_add(4) > string_stream_len)
            {
                continue;
            }
            if properties.lookup_string(offset)?.buffer() == wanted.as_slice() {
                // NameIdEntry::prop_id() already is 0x8000 + wPropIdx.
                return Ok(entry.prop_id());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "the named-property map has no PS_PUBLIC_STRINGS \"Keywords\" entry; this writer does not add map entries, so categories cannot be written",
    ))
}

/// The number of existing contents rows without `MSGFLAG_READ`, read from the rows' PidTagMessageFlags cells (the row
/// matrix is already in memory). The stored PidTagContentUnreadCount is not trusted: in a file this writer did not
/// create it can be stale, negative, missing or of another type. A count that cannot be established is an error, never
/// a guess.
fn unread_rows(table: &dyn TableContext) -> io::Result<i32> {
    let context = table.context();
    let column = context
        .columns()
        .iter()
        .find(|column| column.prop_id() == 0x0E07 && column.prop_type() == PropertyType::Integer32);
    let mut unread = 0i32;
    for row in table.rows_matrix() {
        let Some(column) = column else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot recount PidTagContentUnreadCount: the contents table has no Integer32 PidTagMessageFlags column",
            ));
        };
        // Only the flags cell is decoded (a full row decode per row and commit made the recount quadratic in cost).
        match row.column_value(context, column)? {
            Some(TableRowColumnValue::Small(PropertyValue::Integer32(flags))) => {
                if flags & 0x1 == 0 {
                    unread = unread.checked_add(1).ok_or(PstError::IntegerConversion)?;
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cannot recount PidTagContentUnreadCount: contents row {:#x} has no PidTagMessageFlags value",
                        u32::from(row.id())
                    ),
                ))
            }
        }
    }
    Ok(unread)
}

/// The children of `parent` by exact display name (read once per session). Folder identity is the exact display name:
/// "Inbox" and "inbox" are distinct folders. When two children share a name, the first in the hierarchy table's row
/// order is the one found.
/// Tests: the child of `parent` named `name`, as the session's lookup finds it.
#[cfg(test)]
fn find_child_folder(
    store: &Rc<UnicodeStore>,
    parent: NodeId,
    name: &str,
) -> io::Result<Option<NodeId>> {
    Ok(child_folder_names(store, parent)?.get(name).copied())
}

fn child_folder_names(
    store: &Rc<UnicodeStore>,
    parent: NodeId,
) -> io::Result<HashMap<String, NodeId>> {
    let mut names = HashMap::new();
    let parent = store.open_folder(&store.properties().make_entry_id(parent)?)?;
    let Some(hierarchy) = parent.hierarchy_table() else {
        return Ok(names);
    };
    for row in hierarchy.rows_matrix() {
        let child = NodeId::from(u32::from(row.id()));
        let child_folder = store.open_folder(&store.properties().make_entry_id(child)?)?;
        if let Ok(display_name) = child_folder.properties().display_name() {
            names.entry(display_name.to_string()).or_insert(child);
        }
    }
    Ok(names)
}

fn new_folder_table_cell(
    column: &TableColumnDescriptor,
    store_record_key: [u8; 16],
    folder: NodeId,
    name: &str,
    has_subfolders: bool,
    row_version: u32,
) -> io::Result<Option<TableCell>> {
    let prop_type = column.prop_type();
    let cell = match column.prop_id() {
        LTP_ROW_ID_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(u32::from(folder) as i32),
        )),
        LTP_ROW_VERSION_PROP_ID if prop_type == PropertyType::Integer32 => Some(TableCell::Value(
            PropertyValue::Integer32(row_version as i32),
        )),
        0x3001 => string_table_cell(prop_type, name),
        0x3602 | 0x3603 if prop_type == PropertyType::Integer32 => {
            Some(TableCell::Value(PropertyValue::Integer32(0)))
        }
        0x360A if prop_type == PropertyType::Boolean => {
            Some(TableCell::Value(PropertyValue::Boolean(has_subfolders)))
        }
        0x3613 => string_table_cell(prop_type, "IPF.Note"),
        0x0FFF if prop_type == PropertyType::Binary => {
            let entry_id = EntryId::new(StoreRecordKey::new(store_record_key), folder);
            Some(TableCell::Bytes(Vec::try_from(&entry_id)?))
        }
        _ => None,
    };
    Ok(cell)
}

fn rebuild_hierarchy_table(
    table: &dyn TableContext,
    store_record_key: [u8; 16],
    folder: NodeId,
    name: &str,
    has_subfolders: bool,
    rows_node: NodeId,
    row_version: u32,
) -> io::Result<RebuiltTable> {
    let cells = table
        .context()
        .columns()
        .iter()
        .map(|column| {
            new_folder_table_cell(
                column,
                store_record_key,
                folder,
                name,
                has_subfolders,
                row_version,
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    rebuild_table(
        table,
        vec![(folder, cells)],
        Some(rows_node),
        row_version,
        None,
        true,
    )
}

fn table_rows_node(
    context: &TableContextInfo,
    subnodes: &[UnicodeLeafSubNodeTreeEntry],
) -> io::Result<NodeId> {
    context
        .rows()
        .filter(|node| !matches!(node.id_type(), Ok(NodeIdType::HeapNode)))
        .map(Ok)
        .unwrap_or_else(|| {
            let index = subnodes
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.node().id_type(),
                        Ok(NodeIdType::Internal | NodeIdType::ListsTablesProperties)
                    )
                })
                .map(|entry| entry.node().index())
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(LTP_SUBNODE, index)?)
        })
}

fn writable_folder_properties(folder: &dyn Folder) -> Vec<(u16, PropertyValue)> {
    folder
        .properties()
        .iter()
        .filter(|(id, _)| !matches!(**id, 0x0FFF | 0x3601))
        .map(|(id, value)| (*id, value.clone()))
        .collect()
}

fn set_folder_integer_property(properties: &mut Vec<(u16, PropertyValue)>, id: u16, value: i32) {
    if let Some((_, property)) = properties.iter_mut().find(|(property, _)| *property == id) {
        *property = PropertyValue::Integer32(value);
    } else {
        properties.push((id, PropertyValue::Integer32(value)));
    }
}

fn set_folder_boolean_property(properties: &mut Vec<(u16, PropertyValue)>, id: u16, value: bool) {
    if let Some((_, property)) = properties.iter_mut().find(|(property, _)| *property == id) {
        *property = PropertyValue::Boolean(value);
    } else {
        properties.push((id, PropertyValue::Boolean(value)));
    }
}

fn collect_subnode_entries(
    file: &mut File,
    bbt_ref: UnicodePageRef,
    subnode: Option<UnicodeBlockId>,
) -> io::Result<Vec<UnicodeLeafSubNodeTreeEntry>> {
    let Some(subnode) = subnode else {
        return Ok(Vec::new());
    };
    let bbt = UnicodeBlockBTree::read(file, bbt_ref)?;
    let mut cache = Default::default();
    let block = bbt.find_entry(file, subnode.search_key(), &mut cache)?;
    let tree = SubNodeTree::<UnicodePstFile>::read(file, &block)?;
    Ok(tree.entries(file, &bbt, &mut cache)?.collect())
}

fn write_pending_blocks(
    file: &mut File,
    writer: &mut BlockWriter,
    tx: &mut Tx<'_>,
    blocks: &mut Vec<NewBlock>,
    space: &mut FileSpace,
) -> io::Result<()> {
    for block in blocks.drain(..) {
        if block.precounted {
            tx.changes.precounted.insert(block.id.search_key());
        }
        tx.put_block(new_block_entry(&block));
        writer.push(file, block)?;
    }
    // New entries move into the block B-tree's right edge (full pages are written), and the map pages of complete new
    // ranges are written, so neither accumulates over a large commit.
    tx.drain_appends(file, space)?;
    if let Some(record) = space.record.as_mut() {
        record.write_complete(file)?;
    }
    Ok(())
}

fn append_search_updates(
    file: &mut File,
    store: &Rc<UnicodeStore>,
    tx: &mut Tx<'_>,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    updates: impl IntoIterator<Item = SearchUpdateData>,
) -> io::Result<()> {
    let Some(queue_entry) = tx.node(NID_SEARCH_MANAGEMENT_QUEUE)? else {
        return Ok(());
    };
    let updates: Vec<SearchUpdate> = updates.into_iter().map(SearchUpdate::new).collect();
    // Append to the queue in place. When the queue starts at offset 0 (its nidParent; a reader skips that many bytes)
    // and every leaf but the last is full, only the last leaf, the index blocks on the path to it and the new leaves
    // are written; the stream holds the same bytes as the full rewrite below gives. The reused blocks keep their IDs
    // and their reference counts are not walked, so a commit costs O(new records) plus at most two index blocks
    // (bounded by the 1,021-entry fan-out of MS-PST data trees).
    let start = queue_entry.parent().map(u32::from).unwrap_or_default();
    if start == 0 && queue_entry.data().search_key() != 0 {
        let mut appended = Vec::new();
        for update in &updates {
            update.write(&mut appended)?;
        }
        if let Some(data) = append_to_data_tree(
            file,
            tx,
            blocks,
            next_bid,
            next_offset,
            queue_entry.data(),
            &appended,
            20,
        )? {
            tx.put_node(UnicodeNodeBTreeEntry::new(
                NID_SEARCH_MANAGEMENT_QUEUE,
                data,
                queue_entry.sub_node(),
                queue_entry.parent(),
            ));
            return Ok(());
        }
    }
    let mut data = Vec::new();
    for update in store
        .search_update_queue()?
        .updates()
        .iter()
        .copied()
        .chain(updates)
    {
        update.write(&mut data)?;
    }
    let data = push_data_tree(blocks, next_bid, next_offset, &data)?;
    tx.put_node(UnicodeNodeBTreeEntry::new(
        NID_SEARCH_MANAGEMENT_QUEUE,
        data,
        queue_entry.sub_node(),
        queue_entry.parent(),
    ));
    Ok(())
}

/// Append `extra` to the data tree at `root` when every leaf but the last is full and the stream holds whole records of
/// `record` bytes: only the last leaf, the index blocks on the path to it and the new leaves are written, so the work
/// is bounded by the data-tree fan-out (1,021 entries per index block, two levels), not by the stream's length. Leaves
/// and index blocks that stay keep their IDs. Every reference the new tree adds or drops is stated in `tx.changes`
/// (`precounted`, `released_handled`, `explicit`), so the reference counting does not walk the unchanged children.
/// `None`: the tree has another shape, and the caller rewrites the stream.
#[allow(clippy::too_many_arguments)]
fn append_to_data_tree(
    file: &mut File,
    tx: &mut Tx<'_>,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    root: UnicodeBlockId,
    extra: &[u8],
    record: usize,
) -> io::Result<Option<UnicodeBlockId>> {
    let full = MAX_DATA_BLOCK_SIZE as u64;
    let root_entry = bbt_entry(tx, root)?;
    // The path to the last leaf: the root's level, its children, and (for two levels) the last index block's children.
    let (level, total, outer, inner) = match UnicodeDataTree::read(file, tx.crypt(), &root_entry)? {
        DataTree::Leaf(_) => {
            let mut data = read_data_block(file, tx, root)?;
            if data.len() % record != 0 {
                return Ok(None);
            }
            data.extend_from_slice(extra);
            return push_data_tree(blocks, next_bid, next_offset, &data).map(Some);
        }
        DataTree::Intermediate(tree) => {
            let children: Vec<UnicodeBlockId> =
                tree.entries().iter().map(|entry| entry.block()).collect();
            let (level, total) = (tree.header().level(), u64::from(tree.header().total_size()));
            match level {
                1 => (1, total, Vec::new(), children),
                2 => {
                    let last = *children.last().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "empty data tree index block")
                    })?;
                    let last_entry = bbt_entry(tx, last)?;
                    match UnicodeDataTree::read(file, tx.crypt(), &last_entry)? {
                        DataTree::Intermediate(inner) if inner.header().level() == 1 => {
                            let inner_total = u64::from(inner.header().total_size());
                            // Every index block but the last holds 1,021 full leaves.
                            if total - inner_total != (children.len() as u64 - 1) * 1021 * full {
                                return Ok(None);
                            }
                            (
                                2,
                                total,
                                children,
                                inner.entries().iter().map(|entry| entry.block()).collect(),
                            )
                        }
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            }
        }
    };
    if total % record as u64 != 0 || inner.is_empty() {
        return Ok(None);
    }
    let tail = *inner.last().expect("checked");
    let tail_data = read_data_block(file, tx, tail)?;
    // Every leaf of the last index block but its last is full.
    let inner_total: u64 = (inner.len() as u64 - 1) * full + tail_data.len() as u64;
    if level == 1 && inner_total != total
        || level == 2 && total - inner_total != (outer.len() as u64 - 1) * 1021 * full
    {
        return Ok(None);
    }
    let mut data = tail_data;
    data.extend_from_slice(extra);
    let mut leaves: Vec<(UnicodeBlockId, u32)> = inner[..inner.len() - 1]
        .iter()
        .map(|id| (*id, MAX_DATA_BLOCK_SIZE as u32))
        .collect();
    for chunk in data.chunks(MAX_DATA_BLOCK_SIZE) {
        let size = u32::try_from(chunk.len()).map_err(|_| PstError::IntegerConversion)?;
        let block = push_block(
            blocks,
            next_bid,
            next_offset,
            false,
            NewBlockData::Data(chunk.to_vec()),
        )?;
        tx.changes.explicit.push((block, 1));
        leaves.push((block, size));
    }
    tx.changes.explicit.push((tail, -1));
    tx.changes.released_handled.insert(root.search_key());
    // The new index blocks over the last part (reused leaves count unchanged: the old index block is released without
    // walking its children).
    let mut index_blocks: Vec<(UnicodeBlockId, u32)> = Vec::new();
    for chunk in leaves.chunks(1021) {
        let size = chunk.iter().try_fold(0_u32, |sum, (_, size)| {
            sum.checked_add(*size).ok_or(PstError::IntegerConversion)
        })?;
        let entries = chunk
            .iter()
            .map(|(id, _)| UnicodeDataTreeEntry::from(*id))
            .collect();
        let id = push_block(
            blocks,
            next_bid,
            next_offset,
            true,
            NewBlockData::DataTree(entries, 1, size),
        )?;
        tx.changes.precounted.insert(id.search_key());
        index_blocks.push((id, size));
    }
    let mut top: Vec<(UnicodeBlockId, u32)> = Vec::new();
    if level == 2 {
        let last_inner = *outer.last().expect("two levels");
        tx.changes.explicit.push((last_inner, -1));
        tx.changes.released_handled.insert(last_inner.search_key());
        top.extend(
            outer[..outer.len() - 1]
                .iter()
                .map(|id| (*id, (1021 * MAX_DATA_BLOCK_SIZE) as u32)),
        );
    }
    if level == 1 && index_blocks.len() == 1 {
        return Ok(Some(index_blocks[0].0));
    }
    for (id, _) in &index_blocks {
        tx.changes.explicit.push((*id, 1));
    }
    top.extend(index_blocks);
    if top.len() > 1021 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the data tree exceeds two levels of 1,021 entries",
        ));
    }
    let size = top.iter().try_fold(0_u32, |sum, (_, size)| {
        sum.checked_add(*size).ok_or(PstError::IntegerConversion)
    })?;
    let entries = top
        .iter()
        .map(|(id, _)| UnicodeDataTreeEntry::from(*id))
        .collect();
    let id = push_block(
        blocks,
        next_bid,
        next_offset,
        true,
        NewBlockData::DataTree(entries, 2, size),
    )?;
    tx.changes.precounted.insert(id.search_key());
    Ok(Some(id))
}

fn record_block_reference(
    block: UnicodeBlockId,
    references: &mut BTreeMap<u64, u16>,
    pending: &mut VecDeque<UnicodeBlockId>,
) -> io::Result<()> {
    let key = block.search_key();
    if key == 0 {
        return Ok(());
    }
    let count = references.entry(key).or_default();
    if *count == 0 {
        pending.push_back(block);
    }
    *count = count.checked_add(1).ok_or(PstError::IntegerConversion)?;
    Ok(())
}

fn internal_block_children(
    file: &mut File,
    entry: &UnicodeBlockBTreeEntry,
) -> io::Result<Vec<UnicodeBlockId>> {
    file.seek(SeekFrom::Start(entry.block().index().index()))?;
    let mut block_type = [0];
    file.read_exact(&mut block_type)?;
    Ok(match block_type[0] {
        0x01 => match UnicodeDataTree::read(file, NdbCryptMethod::None, entry)? {
            DataTree::Intermediate(tree) => {
                tree.entries().iter().map(|entry| entry.block()).collect()
            }
            DataTree::Leaf(_) => unreachable!(),
        },
        0x02 => match UnicodeSubNodeTree::read(file, entry)? {
            SubNodeTree::Intermediate(tree) => {
                tree.entries().iter().map(|entry| entry.block()).collect()
            }
            SubNodeTree::Leaf(tree) => tree
                .entries()
                .iter()
                .flat_map(|entry| [Some(entry.block()), entry.sub_node()])
                .flatten()
                .collect(),
        },
        value => return Err(NdbError::InvalidInternalBlockType(value).into()),
    })
}

fn compact_unicode_block_tree(
    file: &mut File,
    entries: &mut Vec<UnicodeBlockBTreeEntry>,
    nodes: &[UnicodeNodeBTreeEntry],
) -> io::Result<()> {
    entries.sort_by_key(BTreeEntry::key);
    let mut references = BTreeMap::new();
    let mut pending = VecDeque::new();
    for node in nodes {
        record_block_reference(node.data(), &mut references, &mut pending)?;
        if let Some(block) = node.sub_node() {
            record_block_reference(block, &mut references, &mut pending)?;
        }
    }

    while let Some(block) = pending.pop_front() {
        let entry = entries
            .binary_search_by_key(&block.search_key(), BTreeEntry::key)
            .ok()
            .and_then(|index| entries.get(index))
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("referenced PST block {:X} is missing", block.search_key()),
                )
            })?;
        if !block.is_internal() {
            continue;
        }
        for child in internal_block_children(file, &entry)? {
            record_block_reference(child, &mut references, &mut pending)?;
        }
    }

    for entry in &mut *entries {
        if let Some(references) = references.get(&entry.key()) {
            entry.set_ref_count(
                references
                    .checked_add(1)
                    .ok_or(PstError::IntegerConversion)?,
            );
        }
    }
    entries.retain(|entry| references.contains_key(&entry.key()));
    Ok(())
}

fn set_unicode_store_display_name(core: &mut WriterCore, name: &str) -> io::Result<()> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "store display name must not be empty",
        ));
    }
    core.run(|mut file, state, store| {
        let header = state.header.clone();
        let old_root = header.root().clone();
        let mut tx = Tx::new(state);
        let store_entry = tx.node(NID_MESSAGE_STORE)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "message store node not found")
        })?;
        let mut properties: Vec<_> = store
            .properties()
            .iter()
            .filter(|(id, _)| **id != 0x3001)
            .map(|(id, value)| (*id, value.clone()))
            .collect();
        properties.push((
            0x3001,
            PropertyValue::Unicode(UnicodeValue::new(name.encode_utf16().collect())),
        ));

        let mut next_bid = header
            .next_block()
            .index()
            .max(tx.max_block_index().unwrap_or(0).saturating_add(1));
        let mut next_offset = tx.file_space();
        let mut blocks = Vec::new();
        let mut subnodes =
            collect_subnode_entries(&mut file, *old_root.block_btree(), store_entry.sub_node())?;
        let next_property_subnode = subnodes
            .iter()
            .filter(|entry| {
                matches!(
                    entry.node().id_type(),
                    Ok(NodeIdType::Internal | NodeIdType::ListsTablesProperties)
                )
            })
            .map(|entry| entry.node().index())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(PstError::IntegerConversion)?;
        let (pages, external_properties) =
            property_context_pages(properties, next_property_subnode)?;
        let data = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, pages)?;
        for (node, value) in external_properties {
            let value = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &value)?;
            subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, value, None));
        }
        let subnode = if subnodes.is_empty() {
            None
        } else {
            Some(push_subnode_tree(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                subnodes,
            )?)
        };
        tx.put_node(UnicodeNodeBTreeEntry::new(
            NID_MESSAGE_STORE,
            data,
            subnode,
            store_entry.parent(),
        ));

        Ok((
            CommitPlan {
                header,
                changes: tx.changes,
                blocks,
                next_bid,
                next_offset,
                reserved_nodes: Vec::new(),
            },
            (),
        ))
    })
}

fn create_unicode_folder(core: &mut WriterCore, parent: NodeId, name: &str) -> io::Result<NodeId> {
    // The session's append state of the parent's hierarchy table, as for contents tables.
    let cached = core.hierarchy.remove(&u32::from(parent));
    let (folder, hierarchy_state, grandparent_roots) =
        core.run(move |mut file, state, store| {
            let header = state.header.clone();
            let old_root = header.root().clone();
            let mut tx = Tx::new(state);
            let store_record_key = *store.properties().record_key()?.record_key();
            let parent_folder = store.open_folder(&store.properties().make_entry_id(parent)?)?;
            let parent_has_subfolders = parent_folder.properties().has_sub_folders()?;
            let parent_entry = tx.node(parent)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "parent folder node not found")
            })?;
            let mut parent_properties = writable_folder_properties(parent_folder.as_ref());
            if !parent_has_subfolders {
                set_folder_boolean_property(&mut parent_properties, 0x360A, true);
            }
            let mut parent_subnodes = collect_subnode_entries(
                &mut file,
                *old_root.block_btree(),
                parent_entry.sub_node(),
            )?;
            let parent_row_hierarchy = (!parent_has_subfolders)
                .then(|| -> io::Result<_> {
                    let grandparent = parent_entry.parent().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "parent folder has no parent")
                    })?;
                    let node = NodeId::new(NodeIdType::HierarchyTable, grandparent.index())?;
                    let entry = tx.node(node)?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "parent hierarchy node not found")
                    })?;
                    let subnodes = collect_subnode_entries(
                        &mut file,
                        *old_root.block_btree(),
                        entry.sub_node(),
                    )?;
                    Ok((grandparent, node, entry, subnodes))
                })
                .transpose()?;

            let folder_index = tx
                .max_node_index(NodeIdType::NormalFolder)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(PstError::IntegerConversion)?
                .max(header.next_node_index(NodeIdType::NormalFolder)?);
            let folder = NodeId::new(NodeIdType::NormalFolder, folder_index)?;
            let hierarchy = NodeId::new(NodeIdType::HierarchyTable, parent.index())?;
            let hierarchy_entry = tx.node(hierarchy)?;
            let hierarchy_template = tx.node(NodeId::from(0x60D))?.map(|entry| entry.data());
            let contents_template = tx.node(NodeId::from(0x60E))?.map(|entry| entry.data());
            let associated_contents_template =
                tx.node(NodeId::from(0x60F))?.map(|entry| entry.data());

            let mut next_bid = header
                .next_block()
                .index()
                .max(tx.max_block_index().unwrap_or(0).saturating_add(1));
            let mut next_offset = tx.file_space();
            let mut blocks = Vec::new();

            let mut new_hierarchy_state = None;
            let (hierarchy_data_bid, hierarchy_subnode_bid) = if let Some(hierarchy_entry) =
                hierarchy_entry
            {
                let mut hierarchy_subnodes = collect_subnode_entries(
                    &mut file,
                    *old_root.block_btree(),
                    hierarchy_entry.sub_node(),
                )?;
                // The new row is appended along paths with the session's state of the table; the table is read in full only
                // to build that state (once per parent and session) or to rebuild a table whose rows are in the heap or
                // absent.
                let read_table = || {
                    parent_folder.hierarchy_table().cloned().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "parent hierarchy table not found")
                    })
                };
                let had_state = cached.is_some();
                let cached = cached.filter(|state| {
                    state.heap_root == hierarchy_entry.data()
                        && hierarchy_subnodes
                            .iter()
                            .any(|entry| entry.block() == state.rows_root)
                });
                if had_state && cached.is_none() {
                    // A state the session kept no longer describes the table: some commit changed it without updating it.
                    crate::session::count_hierarchy_fallback();
                }
                let mut state = match cached {
                    Some(state) => Some(state),
                    None => {
                        let table = read_table()?;
                        let rows_node = table_rows_node(table.context(), &hierarchy_subnodes)?;
                        let rows_block = hierarchy_subnodes
                            .iter()
                            .find(|entry| entry.node() == rows_node)
                            .map(|entry| entry.block());
                        load_contents_state(table.as_ref(), hierarchy_entry.data(), rows_block, 0)?
                    }
                };
                let mut appended = None;
                if let Some(state) = state.as_mut().filter(|_| hierarchy_path_edits()) {
                    if let Some(view) = open_contents_append(&mut file, &tx, state)? {
                        let rows_node = view
                            .context
                            .rows()
                            .ok_or_else(|| io::Error::other("table rows node is missing"))?;
                        let mut rows = vec![(
                            folder,
                            view.context
                                .columns()
                                .iter()
                                .map(|column| {
                                    new_folder_table_cell(
                                        column,
                                        store_record_key,
                                        folder,
                                        name,
                                        false,
                                        header.unique_value(),
                                    )
                                })
                                .collect::<io::Result<Vec<_>>>()?,
                        )];
                        appended = append_rows_to_state(
                            &mut file,
                            &mut tx,
                            state,
                            Some(view),
                            &mut rows,
                            header.unique_value(),
                            &mut blocks,
                            &mut next_bid,
                            &mut next_offset,
                        )?
                        .map(|(heap, rows, _)| (heap, rows_node, rows));
                    }
                    if appended.is_none() {
                        crate::session::count_hierarchy_fallback();
                    }
                }
                let (data, rows_node, rows) = match appended {
                    Some(appended) => {
                        new_hierarchy_state = state;
                        appended
                    }
                    None => {
                        let table = read_table()?;
                        let rows_node = table_rows_node(table.context(), &hierarchy_subnodes)?;
                        let (pages, rows, row_ids) = rebuild_hierarchy_table(
                            table.as_ref(),
                            store_record_key,
                            folder,
                            name,
                            false,
                            rows_node,
                            header.unique_value(),
                        )?;
                        let data = push_data_tree_chunks(
                            &mut blocks,
                            &mut next_bid,
                            &mut next_offset,
                            pages,
                        )?;
                        let rows = push_data_tree_chunks(
                            &mut blocks,
                            &mut next_bid,
                            &mut next_offset,
                            rows,
                        )?;
                        new_hierarchy_state = Some(ContentsState {
                            row_count: row_ids.len(),
                            last_id: row_ids.iter().map(|id| u32::from(*id)).max(),
                            unread: 0,
                            heap_root: data,
                            rows_root: rows,
                        });
                        (data, rows_node, rows)
                    }
                };
                hierarchy_subnodes.retain(|entry| entry.node() != rows_node);
                hierarchy_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows, None));
                let subnodes = push_subnode_tree(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    hierarchy_subnodes,
                )?;
                (data, Some(subnodes))
            } else {
                let data = push_block(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    false,
                    NewBlockData::Data(table_context_data(&[folder])?),
                )?;
                (data, None)
            };

            let (parent_data, parent_subnode) = if parent_has_subfolders {
                (parent_entry.data(), parent_entry.sub_node())
            } else {
                let next_property_subnode = parent_subnodes
                    .iter()
                    .filter(|entry| {
                        matches!(
                            entry.node().id_type(),
                            Ok(NodeIdType::Internal | NodeIdType::ListsTablesProperties)
                        )
                    })
                    .map(|entry| entry.node().index())
                    .max()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or(PstError::IntegerConversion)?;
                let (parent_pages, external_properties) =
                    property_context_pages(parent_properties, next_property_subnode)?;
                let parent_data = push_data_tree_chunks(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    parent_pages,
                )?;
                for (node, data) in external_properties {
                    let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
                    parent_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
                }
                let parent_subnode = if parent_subnodes.is_empty() {
                    None
                } else {
                    Some(push_subnode_tree(
                        &mut blocks,
                        &mut next_bid,
                        &mut next_offset,
                        parent_subnodes,
                    )?)
                };
                (parent_data, parent_subnode)
            };

            let folder_data_bid = push_block(
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                false,
                NewBlockData::Data(folder_data(name, 0, false)?),
            )?;
            let folder_hierarchy = NodeId::new(NodeIdType::HierarchyTable, folder.index())?;
            let folder_hierarchy_bid = match hierarchy_template {
                Some(template) => template,
                None => push_block(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    false,
                    NewBlockData::Data(table_context_data(&[])?),
                )?,
            };
            let folder_contents = NodeId::new(NodeIdType::ContentsTable, folder.index())?;
            let folder_contents_bid = match contents_template {
                Some(template) => template,
                None => push_block(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    false,
                    NewBlockData::Data(table_context_data(&[])?),
                )?,
            };
            let folder_associated_contents =
                NodeId::new(NodeIdType::AssociatedContentsTable, folder.index())?;
            let folder_associated_contents_bid = match associated_contents_template {
                Some(template) => template,
                None => push_block(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    false,
                    NewBlockData::Data(table_context_data(&[])?),
                )?,
            };

            // The grandparent's row of the parent gains PidTagSubfolders, patched through the row index.
            let mut grandparent_roots = None;
            if let Some((grandparent, node, entry, subnodes)) = parent_row_hierarchy {
                let has_subfolders_cell = [(0x360A, PropertyValue::Boolean(true))];
                let view = if hierarchy_path_edits() {
                    open_table_view(&mut file, &tx, entry.data(), |rows| {
                        subnodes
                            .iter()
                            .find(|entry| entry.node() == rows)
                            .map(|entry| entry.block())
                    })?
                } else {
                    None
                };
                let patched = match view {
                    Some(view) => patch_table_row_by_index(
                        &mut file,
                        &mut tx,
                        entry.data(),
                        view,
                        parent,
                        &has_subfolders_cell,
                        header.unique_value(),
                        &mut blocks,
                        &mut next_bid,
                        &mut next_offset,
                    )?,
                    None => None,
                };
                let patched = match patched {
                    Some(patched) => patched,
                    None => {
                        crate::session::count_hierarchy_fallback();
                        let table = store
                            .open_folder(&store.properties().make_entry_id(grandparent)?)?
                            .hierarchy_table()
                            .cloned()
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::NotFound,
                                    "parent hierarchy table not found",
                                )
                            })?;
                        rebuild_hierarchy_row(
                            &mut blocks,
                            &mut next_bid,
                            &mut next_offset,
                            table.as_ref(),
                            &subnodes,
                            parent,
                            &has_subfolders_cell,
                            header.unique_value(),
                        )?
                    }
                };
                let entry = hierarchy_entry_after(
                    &mut blocks,
                    &mut next_bid,
                    &mut next_offset,
                    node,
                    entry,
                    subnodes,
                    patched,
                )?;
                grandparent_roots =
                    Some((grandparent, entry.data(), patched.1.map(|(_, rows)| rows)));
                tx.put_node(entry);
            }
            for entry in [
                UnicodeNodeBTreeEntry::new(
                    parent,
                    parent_data,
                    parent_subnode,
                    parent_entry.parent(),
                ),
                UnicodeNodeBTreeEntry::new(
                    hierarchy,
                    hierarchy_data_bid,
                    hierarchy_subnode_bid,
                    hierarchy_entry.and_then(|entry| entry.parent()),
                ),
                UnicodeNodeBTreeEntry::new(folder, folder_data_bid, None, Some(parent)),
                UnicodeNodeBTreeEntry::new(folder_hierarchy, folder_hierarchy_bid, None, None),
                UnicodeNodeBTreeEntry::new(folder_contents, folder_contents_bid, None, None),
                UnicodeNodeBTreeEntry::new(
                    folder_associated_contents,
                    folder_associated_contents_bid,
                    None,
                    None,
                ),
            ] {
                tx.put_node(entry);
            }

            append_search_updates(
                &mut file,
                &store,
                &mut tx,
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
                [
                    SearchUpdateData::FolderAdded {
                        parent,
                        folder,
                        reserved1: 0,
                        reserved2: 0,
                    },
                    SearchUpdateData::FolderModified {
                        folder: parent,
                        reserved: 0,
                    },
                ],
            )?;

            Ok((
                CommitPlan {
                    header,
                    changes: tx.changes,
                    blocks,
                    next_bid,
                    next_offset,
                    reserved_nodes: vec![
                        folder,
                        folder_hierarchy,
                        folder_contents,
                        folder_associated_contents,
                    ],
                },
                (folder, new_hierarchy_state, grandparent_roots),
            ))
        })?;
    if let Some(state) = hierarchy_state {
        core.hierarchy.insert(u32::from(parent), state);
    }
    if let Some((grandparent, heap, rows)) = grandparent_roots {
        core.hierarchy_changed(grandparent, heap, rows);
    }
    #[cfg(test)]
    core.assert_hierarchy_matches_disk(parent)?;
    Ok(folder)
}

fn ensure_unicode_folder_path(core: &mut WriterCore, folder_path: &[&str]) -> io::Result<NodeId> {
    validate_folder_path(folder_path)?;
    core.state()?;
    let key: Vec<String> = folder_path.iter().map(|name| name.to_string()).collect();
    if let Some(folder) = core.folders.get(&key) {
        return Ok(*folder);
    }
    let mut store = core.store()?;
    let mut parent = store.properties().ipm_sub_tree_entry_id()?.node_id();
    for name in folder_path {
        // A parent's children are read once per session (each child opened once), then kept by name, so a lookup does
        // not open every sibling.
        if !core.children.contains_key(&u32::from(parent)) {
            let names = child_folder_names(&store, parent)?;
            core.children.insert(u32::from(parent), names);
        }
        if let Some(folder) = core.children[&u32::from(parent)].get(*name) {
            parent = *folder;
            continue;
        }
        // ponytail: one COW commit per missing segment; batch only if deep imports need it.
        let folder = create_unicode_folder(core, parent, name)?;
        if let Some(names) = core.children.get_mut(&u32::from(parent)) {
            names.insert(name.to_string(), folder);
        }
        core.children.insert(u32::from(folder), HashMap::new());
        parent = folder;
        store = core.store()?;
    }
    core.folders.insert(key, parent);
    Ok(parent)
}

/// One-shot append (the pre-session public API): open a writer session, commit once, close it.
fn append_unicode_pst_at(
    path: &Path,
    inputs: &[UnicodePstBatchMessage<'_>],
    folder_path: Option<&[&str]>,
) -> io::Result<()> {
    let mut core = WriterCore::open(path)?;
    append_unicode_pst(&mut core, inputs, folder_path)
}

fn append_unicode_pst(
    core: &mut WriterCore,
    inputs: &[UnicodePstBatchMessage<'_>],
    folder_path: Option<&[&str]>,
) -> io::Result<()> {
    if inputs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bulk append requires at least one message",
        ));
    }
    let requested_folder = folder_path
        .map(|folder_path| ensure_unicode_folder_path(core, folder_path))
        .transpose()?;
    // The session's append state of the target folder's contents table, and the store's PidNameKeywords ID.
    let mut cached = requested_folder.and_then(|folder| core.contents.remove(&u32::from(folder)));
    let keywords_cached = core.keywords;
    // The session's copy of the store's name-to-ID map. It is taken out for the commit and put back, with this commit's
    // additions, only once the commit succeeded; after a failure the next commit reads the map from the file.
    let names_cached = core.names.take();
    let (folder, contents_state, keywords_resolved, hierarchy_roots, names_resolved) = core.run(move |mut file, state, store| {
    let mut header = state.header.clone();
    let old_root = header.root().clone();
    let mut tx = Tx::new(state);
    let store_record_key = *store
        .properties()
        .record_key()
        .map_err(|err| io::Error::new(err.kind(), format!("read store record key: {err}")))?
        .record_key();
    // The default receive folder is looked up only when no folder was requested.
    let folder = match requested_folder {
        Some(folder) => folder,
        None => default_receive_folder(&store, &tx.nodes_of_type(NodeIdType::ReceiveFolderTable)?).map_err(|err| {
            io::Error::new(err.kind(), format!("find receive folder: {err}"))
        })?,
    };
    let target_folder = store
        .open_folder(&store.properties().make_entry_id(folder)?)
        .map_err(|err| io::Error::new(err.kind(), format!("open target folder: {err}")))?;
    // The contents table is read only when the session holds no append state describing its current heap (the first
    // append to the folder in this session, or after a rebuild).
    let contents_node = NodeId::new(NodeIdType::ContentsTable, folder.index())?;
    let contents_now = tx.node(contents_node)?;
    cached = cached.filter(|cached| contents_now.is_some_and(|entry| entry.data() == cached.heap_root));
    // With a session state, the append view reads the table's context and the pages it needs from disk; a table whose
    // layout the view does not take is read in full.
    let mut contents_view = match &cached {
        Some(state) => open_contents_append(&mut file, &tx, state)?,
        None => None,
    };
    if contents_view.is_none() {
        cached = None;
    }
    let read_contents_table = || {
        target_folder.contents_table().cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "target-folder contents table not found",
            )
        })
    };
    let contents_table = match &cached {
        Some(_) => None,
        None => Some(read_contents_table()?),
    };
    let contents_context = match (&contents_view, &contents_table) {
        (Some(view), _) => view.context.clone(),
        (None, Some(table)) => table.context().clone(),
        (None, None) => return Err(io::Error::other("contents table state missing")),
    };
    let mut folder_properties = writable_folder_properties(target_folder.as_ref());
    // Categories: the PidNameKeywords property ID comes from this store's own name-to-id map, resolved once per commit
    // and only when some message carries categories.
    let keywords_tag = if inputs.iter().any(|input| !input.message.categories.is_empty()) {
        let tag = match keywords_cached {
            Some(tag) => tag,
            None => keywords_property_id(&store)?,
        };
        if contents_context.columns().iter().any(|column| column.prop_id() == tag) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("the target contents table has a column {tag:#06x} (PidNameKeywords); writing its cells is not supported"),
            ));
        }
        Some(tag)
    } else {
        None
    };

    // The given named properties' IDs, at every depth, from the store's map, which gains an entry for each name it does
    // not have yet; the map is read only when some message gives a named property.
    let mut name_map = names_cached;
    let mut names = NameIds { keywords: keywords_tag, given: HashMap::new() };
    for input in inputs {
        resolve_named_properties(&input.message, input.attachments, &store, &mut name_map, &mut names, 0)?;
    }

    let message_index = tx
        .max_node_index(NodeIdType::NormalMessage)
        .unwrap_or(0x3F)
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?
        .max(header.next_node_index(NodeIdType::NormalMessage)?);
    let messages = (0..inputs.len())
        .map(|offset| {
            let offset = u32::try_from(offset).map_err(|_| PstError::IntegerConversion)?;
            let index = message_index
                .checked_add(offset)
                .ok_or(PstError::IntegerConversion)?;
            Ok::<_, io::Error>(NodeId::new(NodeIdType::NormalMessage, index)?)
        })
        .collect::<io::Result<Vec<_>>>()?;
    // The commit's row version, taken before the check below so that the check builds each message's parts exactly as
    // they are written. A row version has a fixed width, so no size depends on it.
    header.update_unique();
    inputs
        .iter()
        .enumerate()
        .try_for_each(|(position, input)| {
            prepare_message(&input.message, input.attachments, header.unique_value(), &names).map_err(|err| {
                // The size error keeps its type, now with the message's position.
                if let Some(too_large) = err.get_ref().and_then(|inner| inner.downcast_ref::<UnicodePstMessageTooLarge>()) {
                    return io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        UnicodePstMessageTooLarge { position: position + 1, size: too_large.size },
                    );
                }
                let file_size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                io::Error::new(
                    err.kind(),
                    UnicodePstAppendFailed {
                        position: position + 1,
                        detail: format!(
                            "append message {} of {} ({}) at PST size {file_size}: {err}",
                            position + 1,
                            inputs.len(),
                            input.message.message_id.unwrap_or("no Message-ID"),
                        ),
                    },
                )
            })
        })?;
    let contents = NodeId::new(NodeIdType::ContentsTable, folder.index())?;
    let contents_entry = tx
        .node(contents)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "target contents node not found"))?;
    let folder_entry = tx
        .node(folder)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "target folder node not found"))?;
    // The parent's hierarchy table is opened along paths (its row is found through the row index); it is read in full
    // only when the row patch cannot be applied.
    let read_hierarchy = |parent: NodeId| -> io::Result<Rc<dyn TableContext>> {
        store
            .open_folder(&store.properties().make_entry_id(parent)?)?
            .hierarchy_table()
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "parent hierarchy table not found"))
    };
    let parent_hierarchy = folder_entry
        .parent()
        .map(|parent| -> io::Result<_> {
            let node = NodeId::new(NodeIdType::HierarchyTable, parent.index())?;
            let entry = tx
                .node(node)?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "parent hierarchy node not found")
                })?;
            let subnodes =
                collect_subnode_entries(&mut file, *old_root.block_btree(), entry.sub_node())?;
            let view = if hierarchy_path_edits() {
                open_table_view(&mut file, &tx, entry.data(), |rows| {
                    subnodes.iter().find(|entry| entry.node() == rows).map(|entry| entry.block())
                })?
            } else {
                None
            };
            Ok((parent, node, entry, subnodes, view))
        })
        .transpose()?;
    // A counter column of the parent hierarchy table that is not Integer32 cannot be patched or rebuilt with the new
    // counts (the row patch gives up and the rebuild keeps the old cell), which would leave the row inconsistent with
    // the folder. Refuse before anything is written.
    if let Some((parent, _, _, _, view)) = &parent_hierarchy {
        let context = match view {
            Some(view) => view.context.clone(),
            None => read_hierarchy(*parent)?.context().clone(),
        };
        if let Some(column) = context.columns().iter().find(|column| {
            matches!(column.prop_id(), 0x3602 | 0x3603) && column.prop_type() != PropertyType::Integer32
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "parent hierarchy table column {:#06x} has type {:?}, expected Integer32; the folder counters cannot be kept consistent",
                    column.prop_id(),
                    column.prop_type()
                ),
            ));
        }
    }
    // The existing unread count. The rows' flags cells are recounted before the new rows are added and compared with
    // the stored PidTagContentUnreadCount: a usable stored value (Integer32, not negative) must agree with the recount,
    // otherwise the state is inconsistent and nothing is written (which of the two is wrong cannot be told); an
    // unusable stored value (missing, another type, negative) is replaced by the recount.
    let recounted_unread = match (&cached, &contents_table) {
        (Some(cached), _) => cached.unread,
        (None, Some(table)) => unread_rows(table.as_ref())?,
        (None, None) => return Err(io::Error::other("contents table state missing")),
    };
    let stored_unread = folder_properties.iter().find_map(|(id, value)| match value {
        PropertyValue::Integer32(count) if *id == 0x3603 && *count >= 0 => Some(*count),
        _ => None,
    });
    let existing_unread = match stored_unread {
        Some(stored) if stored != recounted_unread => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stored PidTagContentUnreadCount {stored} disagrees with {recounted_unread} unread contents rows; the folder's counters are inconsistent"
                ),
            ))
        }
        _ => recounted_unread,
    };
    let mut folder_subnodes =
        collect_subnode_entries(&mut file, *old_root.block_btree(), folder_entry.sub_node())?;
    let mut contents_subnodes = collect_subnode_entries(
        &mut file,
        *old_root.block_btree(),
        contents_entry.sub_node(),
    )?;
    let rows_node = table_rows_node(&contents_context, &contents_subnodes)?;

    let mut next_bid = header
        .next_block()
        .index()
        .max(tx.max_block_index().unwrap_or(0).saturating_add(1));
    let mut next_offset = tx.file_space();

    let mut message_entries = Vec::with_capacity(inputs.len());
    let mut message_sizes = Vec::with_capacity(inputs.len());
    let mut message_values: Vec<SharedValues> = Vec::with_capacity(inputs.len());
    let mut block_writer = BlockWriter::new(header.crypt_method());
    for (position, (input, message)) in inputs.iter().zip(&messages).enumerate() {
        let mut blocks = Vec::new();
        let result = (|| {
            let built = build_message(&input.message, input.attachments, header.unique_value(), &names)?;
            message_sizes.push(built.size);
            // Attachment data blocks go to the file while they are produced.
            let mut flush = |pending: &mut Vec<NewBlock>, space: &mut FileSpace| write_pending_blocks(&mut file, &mut block_writer, &mut tx, pending, space);
            let (message_bid, message_subnode_bid, values) =
                push_message(&mut blocks, &mut next_bid, &mut next_offset, built, input.attachments, &mut flush)?;
            message_values.push(values);
            write_pending_blocks(&mut file, &mut block_writer, &mut tx, &mut blocks, &mut next_offset)?;
            Ok::<_, io::Error>(UnicodeNodeBTreeEntry::new(
                *message,
                message_bid,
                Some(message_subnode_bid),
                Some(folder),
            ))
        })();
        message_entries.push(result.map_err(|err| {
            let file_size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            io::Error::new(
                err.kind(),
                UnicodePstAppendFailed {
                    position: position + 1,
                    detail: format!(
                        "append message {} of {} ({}) at PST size {file_size}: {err}",
                        position + 1,
                        inputs.len(),
                        input.message.message_id.unwrap_or("no Message-ID"),
                    ),
                },
            )
        })?);
    }

    // Cell subnodes are NID_TYPE_LTP and the row matrix is internal, so the next index counts every type.
    block_writer.flush(&mut file)?;

    let next_contents_subnode = contents_subnodes
        .iter()
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0)
        .max(rows_node.index())
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?;
    let mut blocks = Vec::new();
    let (contents_data_bid, rows_bid, row_count, external_contents, mut new_contents_state) = (|| {
        let (mut new_rows, external) = new_contents_rows(
            &contents_context,
            inputs,
            store_record_key,
            folder,
            &messages,
            &message_sizes,
            next_contents_subnode,
            header.unique_value(),
            &names,
        )?;
        let old_rows_bid = contents_subnodes
            .iter()
            .find(|entry| entry.node() == rows_node)
            .map(|entry| entry.block());
        let mut table = contents_table.clone();
        let (state, view) = match cached.take().filter(|cached| Some(cached.rows_root) == old_rows_bid) {
            Some(state) => (Some(state), contents_view.take()),
            None => {
                let read = match &table {
                    Some(read) => read.clone(),
                    None => read_contents_table()?,
                };
                let state = load_contents_state(
                    read.as_ref(),
                    contents_entry.data(),
                    old_rows_bid,
                    recounted_unread,
                )?;
                table = Some(read);
                (state, None)
            }
        };
        if let Some(mut state) = state {
            if let Some((heap, rows, count)) = append_rows_to_state(
                &mut file,
                &mut tx,
                &mut state,
                view,
                &mut new_rows,
                header.unique_value(),
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
            )? {
                return Ok((heap, rows, count, external, Some(state)));
            }
        }
        let table = match table {
            Some(table) => table,
            None => read_contents_table()?,
        };
        let (heap, rows, row_ids) = rebuild_table_with_rows_and_version(
            table.as_ref(),
            new_rows,
            Some(rows_node),
            header.unique_value(),
        )?;
        let heap = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, heap)?;
        let rows = push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, rows)?;
        Ok::<_, io::Error>((heap, rows, row_ids.len(), external, None))
    })()
    .map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("rebuild receive-folder contents: {err}"),
        )
    })?;
    contents_subnodes.retain(|entry| entry.node() != rows_node);
    contents_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(rows_node, rows_bid, None));
    for (node, position, prop_id, data) in external_contents {
        // The message's own block for the same value, as Outlook writes a contents cell in a subnode.
        let data = match message_values.get(position).and_then(|values| shared_block(values, prop_id, &data)) {
            Some(block) => block,
            None => push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?,
        };
        contents_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
    }
    let contents_subnode_bid = push_subnode_tree(
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        contents_subnodes,
    )?;

    let content_count = i32::try_from(row_count).map_err(|_| PstError::IntegerConversion)?;
    set_folder_integer_property(&mut folder_properties, 0x3602, content_count);
    // PidTagContentUnreadCount: the existing rows without MSGFLAG_READ (recounted above, the stored value is not
    // trusted) plus the appended rows without it.
    let appended_unread = i32::try_from(
        inputs
            .iter()
            .filter(|input| !input.message.is_read)
            .count(),
    )
    .map_err(|_| PstError::IntegerConversion)?;
    let unread_count = existing_unread
        .checked_add(appended_unread)
        .ok_or(PstError::IntegerConversion)?;
    set_folder_integer_property(&mut folder_properties, 0x3603, unread_count);
    if let Some(state) = new_contents_state.as_mut() {
        state.unread = unread_count;
    }
    let counter_cells = [
        (0x3602, PropertyValue::Integer32(content_count)),
        (0x3603, PropertyValue::Integer32(unread_count)),
    ];
    let next_property_subnode = folder_subnodes
        .iter()
        .filter(|entry| matches!(entry.node().id_type(), Ok(NodeIdType::Internal | NodeIdType::ListsTablesProperties)))
        .map(|entry| entry.node().index())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(PstError::IntegerConversion)?;
    let (folder_pages, external_properties) =
        property_context_pages(folder_properties, next_property_subnode)?;
    let folder_data_bid =
        push_data_tree_chunks(&mut blocks, &mut next_bid, &mut next_offset, folder_pages)?;
    for (node, data) in external_properties {
        let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &data)?;
        folder_subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, data, None));
    }
    let folder_subnode_bid = if folder_subnodes.is_empty() {
        None
    } else {
        Some(push_subnode_tree(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            folder_subnodes,
        )?)
    };

    let mut hierarchy_roots = None;
    if let Some((parent, node, entry, subnodes, view)) = parent_hierarchy {
        let patched = match view {
            Some(view) => patch_table_row_by_index(
                &mut file,
                &mut tx,
                entry.data(),
                view,
                folder,
                &counter_cells,
                header.unique_value(),
                &mut blocks,
                &mut next_bid,
                &mut next_offset,
            )?,
            None => None,
        };
        let patched = match patched {
            Some(patched) => patched,
            None => {
                crate::session::count_hierarchy_fallback();
                let table = read_hierarchy(parent)?;
                rebuild_hierarchy_row(&mut blocks, &mut next_bid, &mut next_offset, table.as_ref(), &subnodes, folder, &counter_cells, header.unique_value())?
            }
        };
        let entry = hierarchy_entry_after(&mut blocks, &mut next_bid, &mut next_offset, node, entry, subnodes, patched)?;
        hierarchy_roots = Some((parent, entry.data(), patched.1.map(|(_, rows)| rows)));
        tx.put_node(entry);
    }
    for entry in [
        UnicodeNodeBTreeEntry::new(
            contents,
            contents_data_bid,
            Some(contents_subnode_bid),
            contents_entry.parent(),
        ),
        UnicodeNodeBTreeEntry::new(
            folder,
            folder_data_bid,
            folder_subnode_bid,
            folder_entry.parent(),
        ),
    ] {
        tx.put_node(entry);
    }
    for entry in message_entries {
        tx.put_node(entry);
    }
    // The name-to-ID map, rewritten in this commit when it gained entries.
    if let Some(map) = name_map.as_ref().filter(|map| map.changed) {
        write_name_to_id_map(&mut tx, &mut blocks, &mut next_bid, &mut next_offset, map)?;
    }

    append_search_updates(
        &mut file,
        &store,
        &mut tx,
        &mut blocks,
        &mut next_bid,
        &mut next_offset,
        messages
            .iter()
            .map(|message| SearchUpdateData::MessageAdded {
                parent: folder,
                message: *message,
            }),
    )?;

    Ok((
        CommitPlan {
            header,
            changes: tx.changes,
            blocks,
            next_bid,
            next_offset,
            reserved_nodes: messages,
        },
        (folder, new_contents_state, keywords_tag, hierarchy_roots, name_map),
    ))
    })?;
    if let Some(state) = contents_state {
        core.contents.insert(u32::from(folder), state);
    }
    if let Some((parent, heap, rows)) = hierarchy_roots {
        core.hierarchy_changed(parent, heap, rows);
    }
    crate::measurement::contents_after_commit(|| core.contents_report());
    #[cfg(test)]
    core.assert_contents_matches_disk(folder)?;
    if keywords_resolved.is_some() {
        core.keywords = keywords_resolved;
    }
    core.names = names_resolved.map(|mut map| {
        map.changed = false;
        map
    });
    Ok(())
}

/// Resolve the named properties of a message and of the messages attached to it, depth first, their rules checked
/// first. The map is read from the store the first time a commit needs it.
fn resolve_named_properties(
    message: &UnicodePstMessage<'_>,
    attachments: &[UnicodePstAttachment<'_>],
    store: &Rc<UnicodeStore>,
    map: &mut Option<NameMap>,
    names: &mut NameIds,
    depth: usize,
) -> io::Result<()> {
    if depth > MAX_ATTACHED_MESSAGE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("attached messages nest deeper than {MAX_ATTACHED_MESSAGE_DEPTH} levels"),
        ));
    }
    if !message.named_properties.is_empty() {
        check_named_properties(message.named_properties)?;
        if map.is_none() {
            *map = Some(NameMap::read(store)?);
        }
        let map = map
            .as_mut()
            .ok_or_else(|| io::Error::other("the name-to-ID map was not read"))?;
        for property in message.named_properties {
            let id = map.resolve(property.set, property.name)?;
            names
                .given
                .insert(name_key(property.set, property.name), id);
        }
    }
    for attachment in attachments {
        if let Some(attached) = attachment.attached_message.as_deref() {
            resolve_named_properties(
                &attached.message,
                &attached.attachments,
                store,
                map,
                names,
                depth + 1,
            )?;
        }
    }
    Ok(())
}

/// The name-to-ID map's node rewritten from the session's copy (MS-PST 2.4.7). The copy holds every value of the map's
/// property context, so the new context, with subnodes for its large values, replaces the old one whole.
fn write_name_to_id_map(
    tx: &mut Tx<'_>,
    blocks: &mut Vec<NewBlock>,
    next_bid: &mut u64,
    next_offset: &mut FileSpace,
    map: &NameMap,
) -> io::Result<()> {
    let entry = tx
        .node(NID_NAME_TO_ID_MAP)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "name-to-ID map node not found"))?;
    let (pages, external) = property_context_pages(map.properties(), 0x80)?;
    let data = push_data_tree_chunks(blocks, next_bid, next_offset, pages)?;
    let mut subnodes = Vec::with_capacity(external.len());
    for (node, value) in external {
        let value = push_data_tree(blocks, next_bid, next_offset, &value)?;
        subnodes.push(UnicodeLeafSubNodeTreeEntry::new(node, value, None));
    }
    let subnode = if subnodes.is_empty() {
        None
    } else {
        Some(push_subnode_tree(blocks, next_bid, next_offset, subnodes)?)
    };
    tx.put_node(UnicodeNodeBTreeEntry::new(
        NID_NAME_TO_ID_MAP,
        data,
        subnode,
        entry.parent(),
    ));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod create_tests {
    use super::*;
    use crate::messaging::attachment::{Attachment, AttachmentData, UnicodeAttachment};
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_RECIPIENTS: &[UnicodePstRecipient<'static>] = &[
        UnicodePstRecipient {
            name: "to recipient",
            email: "to@example.com",
            recipient_type: UnicodePstRecipientType::To,
            properties: &[],
        },
        UnicodePstRecipient {
            name: "cc recipient",
            email: "cc@example.com",
            recipient_type: UnicodePstRecipientType::Cc,
            properties: &[],
        },
        UnicodePstRecipient {
            name: "bcc recipient",
            email: "bcc@example.com",
            recipient_type: UnicodePstRecipientType::Bcc,
            properties: &[],
        },
    ];

    fn last_inbox_message(path: &Path) -> Rc<dyn Message> {
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        store
            .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
            .unwrap()
    }

    fn folder_at_path(store: &Rc<UnicodeStore>, folder_path: &[&str]) -> Rc<dyn Folder> {
        let mut folder = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        for name in folder_path {
            folder = find_child_folder(store, folder, name).unwrap().unwrap();
        }
        store
            .open_folder(&store.properties().make_entry_id(folder).unwrap())
            .unwrap()
    }

    /// Read state, importance (no derived priority), sender address type, sent-representing identity, client submit
    /// time, the attachment hidden flag and folder unread counters are written; the contents-table cells agree with the
    /// message properties; folder names match exactly (a case variant is a distinct folder).
    #[test]
    fn writes_message_state_identity_and_folder_counters_consistently() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("outlook-pst-b1-{}-{stamp}.pst", std::process::id()));
        let first_key = [0xB1_u8; 16];
        let second_key = [0xB2_u8; 16];
        let third_key = [0xB3_u8; 16];
        let fourth_key = [0xB4_u8; 16];
        let first = UnicodePstMessage {
            subject: "B1 first",
            recipients: TEST_RECIPIENTS,
            body: "Body",
            html_body: None,
            message_id: Some("<b1-first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: Some(&first_key),
            is_read: false,
            importance: UnicodePstImportance::High,
            sender: Some(UnicodePstSender {
                name: "Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: Some(133_750_079_000_000_000),
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let inline = UnicodePstAttachment {
            filename: "inline.png",
            mime_type: "image/png",
            content_id: Some("<inline-b1@example.com>"),
            data: b"not really a png",
            stream: None,
            hidden: true,
            attached_message: None,
        };
        let second = UnicodePstMessage {
            subject: "B1 second",
            message_id: Some("<b1-second@example.com>"),
            search_key: Some(&second_key),
            is_read: true,
            importance: UnicodePstImportance::Normal,
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
            ..first
        };
        let third = UnicodePstMessage {
            subject: "B1 third",
            message_id: Some("<b1-third@example.com>"),
            search_key: Some(&third_key),
            importance: UnicodePstImportance::Low,
            ..first
        };
        let fourth = UnicodePstMessage {
            subject: "B1 fourth",
            message_id: Some("<b1-fourth@example.com>"),
            search_key: Some(&fourth_key),
            ..first
        };
        drop(
            UnicodePstFile::create_in_folder_with_attachments(
                &path,
                &["Klasör"],
                &first,
                &[inline],
            )
            .unwrap(),
        );
        // A case variant is a different folder, not a merge target.
        drop(UnicodePstFile::append_in_folder(&path, &["KLASÖR"], &second).unwrap());
        drop(UnicodePstFile::append_in_folder(&path, &["Klasör"], &third).unwrap());
        // The template's Inbox already carries an unread count of 0; it must become 1.
        drop(UnicodePstFile::append(&path, &fourth).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        assert!(find_child_folder(&store, ipm, "klasör").unwrap().is_none());
        let folder = folder_at_path(&store, &["Klasör"]);
        assert_eq!(folder.properties().display_name().unwrap(), "Klasör");
        assert_eq!(folder.properties().content_count().unwrap(), 2);
        assert_eq!(folder.properties().unread_count().unwrap(), 2);
        let variant = folder_at_path(&store, &["KLASÖR"]);
        assert_eq!(variant.properties().display_name().unwrap(), "KLASÖR");
        assert_eq!(variant.properties().content_count().unwrap(), 1);
        assert_eq!(variant.properties().unread_count().unwrap(), 0);
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        assert_eq!(inbox.properties().unread_count().unwrap(), 1);

        // The parent's hierarchy row mirrors both counters of the folder.
        let ipm_folder = store
            .open_folder(&store.properties().make_entry_id(ipm).unwrap())
            .unwrap();
        let hierarchy = ipm_folder.hierarchy_table().unwrap();
        let folder_node = folder.properties().node_id();
        let row = hierarchy
            .find_row(TableRowId::new(u32::from(folder_node)))
            .unwrap();
        let values = row.columns(hierarchy.context()).unwrap();
        let mut seen_counter_columns = 0;
        for (column, value) in hierarchy.context().columns().iter().zip(&values) {
            if !matches!(column.prop_id(), 0x3602 | 0x3603) {
                continue;
            }
            seen_counter_columns += 1;
            let value = hierarchy
                .read_column(value.as_ref().unwrap(), column.prop_type())
                .unwrap();
            assert!(
                matches!(value, PropertyValue::Integer32(2)),
                "hierarchy column {:04X}: {value:?}",
                column.prop_id()
            );
        }
        assert_eq!(
            seen_counter_columns, 2,
            "hierarchy table lacks a counter column"
        );

        let same_value = |cell: &PropertyValue, property: &PropertyValue| match (cell, property) {
            (PropertyValue::Unicode(a), PropertyValue::Unicode(b)) => {
                a.to_string() == b.to_string()
            }
            (a, b) => format!("{a:?}") == format!("{b:?}"),
        };
        let contents = folder.contents_table().unwrap();
        let mut seen_flags_column = false;
        for row in contents.rows_matrix() {
            let message_id = NodeId::from(u32::from(row.id()));
            let entry_id = store.properties().make_entry_id(message_id).unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let properties = message.properties();
            let subject = match properties.get(0x0037).unwrap() {
                PropertyValue::Unicode(value) => value.to_string(),
                other => panic!("unexpected subject value: {other:?}"),
            };
            let (key, importance, flags, submit) = match subject.as_str() {
                "B1 first" => (first_key, 2, 0x10, 133_750_079_000_000_000_i64),
                "B1 third" => (third_key, 0, 0, 133_750_079_000_000_000_i64),
                other => panic!("unexpected message {other}"),
            };
            assert!(
                matches!(properties.get(0x0017), Some(PropertyValue::Integer32(v)) if *v == importance),
                "importance of {subject}: {:?}",
                properties.get(0x0017)
            );
            // PidTagPriority has no input and is not derived from importance.
            assert!(properties.get(0x0026).is_none(), "priority of {subject}");
            assert_eq!(
                properties.message_flags().unwrap(),
                flags,
                "flags of {subject}"
            );
            assert!(
                matches!(properties.get(0x0039), Some(PropertyValue::Time(v)) if *v == submit),
                "submit time of {subject}: {:?}",
                properties.get(0x0039)
            );
            assert!(
                matches!(properties.get(0x0E06), Some(PropertyValue::Time(v)) if *v == 133_750_080_000_000_000),
                "delivery time of {subject}: {:?}",
                properties.get(0x0E06)
            );
            for id in [0x0C1E_u16, 0x0064] {
                match properties.get(id) {
                    Some(PropertyValue::Unicode(value)) => {
                        assert_eq!(value.to_string(), "SMTP", "{id:04X}")
                    }
                    other => panic!("{id:04X} of {subject}: {other:?}"),
                }
            }
            match properties.get(0x0065) {
                Some(PropertyValue::Unicode(value)) => {
                    assert_eq!(value.to_string(), "sender@example.com")
                }
                other => panic!("0065 of {subject}: {other:?}"),
            }
            match properties.get(0x0042) {
                Some(PropertyValue::Unicode(value)) => assert_eq!(value.to_string(), "Sender"),
                other => panic!("0042 of {subject}: {other:?}"),
            }
            match properties.get(0x300B) {
                Some(PropertyValue::Binary(value)) => assert_eq!(value.buffer(), &key),
                other => panic!("300B of {subject}: {other:?}"),
            }

            // Contents-table cells carry the same values as the message properties.
            let values = row.columns(contents.context()).unwrap();
            for (column, value) in contents.context().columns().iter().zip(&values) {
                let id = column.prop_id();
                if !matches!(
                    id,
                    0x0017 | 0x0039 | 0x0064 | 0x0065 | 0x0C1E | 0x0E06 | 0x0E07 | 0x3008 | 0x300B
                ) {
                    continue;
                }
                let value = value
                    .as_ref()
                    .unwrap_or_else(|| panic!("column {id:04X} has no cell for {subject}"));
                let cell = contents.read_column(value, column.prop_type()).unwrap();
                if id == 0x0E07 {
                    seen_flags_column = true;
                }
                let property = properties.get(id).unwrap();
                assert!(
                    same_value(&cell, property),
                    "contents column {id:04X} of {subject}: {cell:?} vs {property:?}"
                );
            }

            if subject == "B1 first" {
                let attachment_id = NodeId::from(u32::from(
                    message
                        .attachment_table()
                        .unwrap()
                        .rows_matrix()
                        .next()
                        .unwrap()
                        .id(),
                ));
                let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
                assert!(matches!(
                    attachment.properties().get(0x7FFE),
                    Some(PropertyValue::Boolean(true))
                ));
            }
        }
        assert!(
            seen_flags_column,
            "the contents table has no PidTagMessageFlags column"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// A read message with an attachment (flags 0x11), Normal importance, no PidTagPriority, and no
    /// PidTagClientSubmitTime when the input has none, on the message and on the contents-table cells the template
    /// provides; the parent's hierarchy row carries content count 1 and unread count 0, so the two counters cannot be
    /// swapped. The test above only reads unread High/Low messages with an explicit submit time.
    #[test]
    fn writes_read_normal_message_without_submit_time() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-b1-read-{}-{stamp}.pst",
            std::process::id()
        ));
        let key = [0xC1_u8; 16];
        let input = UnicodePstMessage {
            subject: "B1 read normal",
            recipients: TEST_RECIPIENTS,
            body: "Body",
            html_body: None,
            message_id: Some("<b1-read@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: Some(&key),
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let attachment = UnicodePstAttachment {
            filename: "note.txt",
            mime_type: "text/plain",
            content_id: None,
            data: b"attachment",
            stream: None,
            hidden: false,
            attached_message: None,
        };
        drop(UnicodePstFile::create_with_attachments(&path, &input, &[attachment]).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        assert_eq!(inbox.properties().unread_count().unwrap(), 0);

        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        let ipm_folder = store
            .open_folder(&store.properties().make_entry_id(ipm).unwrap())
            .unwrap();
        let hierarchy = ipm_folder.hierarchy_table().unwrap();
        let inbox_row = hierarchy
            .find_row(TableRowId::new(u32::from(inbox.properties().node_id())))
            .unwrap();
        let inbox_values = inbox_row.columns(hierarchy.context()).unwrap();
        let mut counters = Vec::new();
        for (column, value) in hierarchy.context().columns().iter().zip(&inbox_values) {
            if matches!(column.prop_id(), 0x3602 | 0x3603) {
                let value = hierarchy
                    .read_column(value.as_ref().unwrap(), column.prop_type())
                    .unwrap();
                counters.push((column.prop_id(), format!("{value:?}")));
            }
        }
        counters.sort();
        assert_eq!(
            counters,
            [
                (0x3602, format!("{:?}", PropertyValue::Integer32(1))),
                (0x3603, format!("{:?}", PropertyValue::Integer32(0))),
            ]
        );

        let contents = inbox.contents_table().unwrap();
        let row = contents.rows_matrix().next().unwrap();
        let entry_id = store
            .properties()
            .make_entry_id(NodeId::from(u32::from(row.id())))
            .unwrap();
        let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
        let properties = message.properties();
        assert_eq!(properties.message_flags().unwrap(), 0x11);
        assert!(matches!(
            properties.get(0x0E1B),
            Some(PropertyValue::Boolean(true))
        ));
        assert!(matches!(
            properties.get(0x0017),
            Some(PropertyValue::Integer32(1))
        ));
        assert!(properties.get(0x0026).is_none());
        assert!(properties.get(0x0039).is_none());
        assert!(matches!(
            properties.get(0x0E06),
            Some(PropertyValue::Time(133_750_080_000_000_000))
        ));
        let values = row.columns(contents.context()).unwrap();
        let mut checked = Vec::new();
        for (column, value) in contents.context().columns().iter().zip(&values) {
            let id = column.prop_id();
            if !matches!(id, 0x0017 | 0x0039 | 0x0E06 | 0x0E07 | 0x3008) {
                continue;
            }
            let cell = value
                .as_ref()
                .map(|value| contents.read_column(value, column.prop_type()).unwrap());
            assert_eq!(
                format!("{cell:?}"),
                format!("{:?}", properties.get(id)),
                "contents column {id:04X}"
            );
            checked.push(id);
        }
        assert_eq!(checked, [0x0017, 0x0039, 0x0E06, 0x0E07, 0x3008]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sets_the_store_display_name_and_keeps_appending() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-store-name-{}-{stamp}.pst",
            std::process::id()
        ));
        let input = UnicodePstMessage {
            subject: "Store name",
            recipients: TEST_RECIPIENTS,
            body: "Body",
            html_body: None,
            message_id: Some("<store-name@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "Memex Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(UnicodePstFile::create(&path, &input).unwrap());
        assert!(UnicodePstFile::set_store_display_name(&path, "").is_err());
        drop(UnicodePstFile::set_store_display_name(&path, "d0104.shin").unwrap());
        drop(UnicodePstFile::append(&path, &input).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        assert_eq!(store.properties().display_name().unwrap(), "d0104.shin");
        assert!(store.properties().record_key().is_ok());
        assert_eq!(
            folder_at_path(&store, &["Inbox"])
                .properties()
                .content_count()
                .unwrap(),
            2
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_a_reopenable_unicode_pst_with_one_message() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-create-{}-{stamp}.pst",
            std::process::id()
        ));

        let input = UnicodePstMessage {
            subject: "Memex PST export",
            recipients: TEST_RECIPIENTS,
            body: "Hello from Memex PST export.",
            html_body: None,
            message_id: Some("<memex-pst-export@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "Memex Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let pst = UnicodePstFile::create(&path, &input).unwrap();
        assert_eq!(pst.header().version(), NdbVersion::Unicode);
        // A new store keeps the template's permute encoding, Outlook's default.
        assert_eq!(pst.header().crypt_method(), NdbCryptMethod::Permute);
        drop(pst);

        let reopened = UnicodePstFile::open(&path).unwrap();
        assert_eq!(reopened.header().version(), NdbVersion::Unicode);
        // rgbFM (0x100..0x180) and rgbFP (0x180..0x200) are deprecated and MUST be 0xFF.
        let header = std::fs::read(&path).unwrap();
        assert!(header[0x100..0x200].iter().all(|byte| *byte == 0xFF));
        let store = UnicodeStore::read(Rc::new(reopened)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().display_name().unwrap(), "Inbox");
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        let contents = inbox.contents_table().unwrap();
        let message_row = contents.rows_matrix().next().unwrap();
        let message_id = NodeId::from(u32::from(message_row.id()));
        let message = store
            .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
            .unwrap();
        match message.properties().get(0x0037).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "Memex PST export"),
            value => panic!("unexpected subject value: {value:?}"),
        }
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "Hello from Memex PST export.")
            }
            value => panic!("unexpected body value: {value:?}"),
        }

        let recipients = message.recipient_table().unwrap();
        let recipient_rows = recipients
            .rows_matrix()
            .map(|row| {
                let values = row.columns(recipients.context()).unwrap();
                let read = |id| {
                    let (column, value) = recipients
                        .context()
                        .columns()
                        .iter()
                        .zip(&values)
                        .find(|(column, _)| column.prop_id() == id)
                        .unwrap();
                    recipients
                        .read_column(value.as_ref().unwrap(), column.prop_type())
                        .unwrap()
                };
                let recipient_type = match read(0x0C15) {
                    PropertyValue::Integer32(value) => value,
                    value => panic!("unexpected recipient type: {value:?}"),
                };
                let smtp = match read(0x39FE) {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected recipient value: {value:?}"),
                };
                (recipient_type, smtp)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recipient_rows,
            [
                (1, "to@example.com".to_string()),
                (2, "cc@example.com".to_string()),
                (3, "bcc@example.com".to_string()),
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn externalizes_large_recipient_display_values_in_contents_table() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-large-display-to-{}-{stamp}.pst",
            std::process::id()
        ));
        let names = (0..1_500)
            .map(|index| format!("수신자 {index:04}"))
            .collect::<Vec<_>>();
        let emails = (0..1_500)
            .map(|index| format!("recipient-{index}@example.com"))
            .collect::<Vec<_>>();
        let recipients = names
            .iter()
            .zip(&emails)
            .map(|(name, email)| UnicodePstRecipient {
                name,
                email,
                recipient_type: UnicodePstRecipientType::To,
                properties: &[],
            })
            .collect::<Vec<_>>();
        let display_to = recipient_display(&recipients, UnicodePstRecipientType::To);
        assert!(utf16_bytes(&display_to).len() > MAX_HN_ALLOCATION_SIZE);
        let input = UnicodePstMessage {
            subject: "large recipient display",
            recipients: &recipients,
            body: "body",
            html_body: None,
            message_id: Some("<large-display-to@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };

        drop(UnicodePstFile::create(&path, &input).unwrap());
        drop(UnicodePstFile::append_many(&path, &[input, input]).unwrap());
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3);
        let contents = inbox.contents_table().unwrap();
        let mut cell_nodes = std::collections::HashSet::new();
        for row in contents.rows_matrix() {
            let values = row.columns(contents.context()).unwrap();
            let (column, value) = contents
                .context()
                .columns()
                .iter()
                .zip(&values)
                .find(|(column, _)| column.prop_id() == 0x0E04)
                .unwrap();
            let Some(TableRowColumnValue::Node(node)) = value else {
                panic!("display-to cell is not in a subnode: {value:?}");
            };
            // Outlook fails GetTable unless cell subnodes are NID_TYPE_LTP, and appends must not reuse a NID.
            assert!(matches!(
                node.id_type(),
                Ok(NodeIdType::ListsTablesProperties)
            ));
            assert!(
                cell_nodes.insert(u32::from(*node)),
                "duplicate cell subnode"
            );
            match contents
                .read_column(value.as_ref().unwrap(), column.prop_type())
                .unwrap()
            {
                PropertyValue::Unicode(value) => assert_eq!(value.to_string(), display_to),
                value => panic!("unexpected display-to value: {value:?}"),
            }
        }
        let message = last_inbox_message(&path);
        assert_eq!(
            message.recipient_table().unwrap().rows_matrix().count(),
            recipients.len()
        );
        drop((message, inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn appends_contents_rows_incrementally_without_leaking_heap() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = |name: &str| {
            std::env::temp_dir().join(format!(
                "outlook-pst-incremental-{name}-{}-{stamp}.pst",
                std::process::id()
            ))
        };
        let ids = (0..1_700)
            .map(|index| format!("<incremental-{index}@example.com>"))
            .collect::<Vec<_>>();
        let message = |index: usize| UnicodePstMessage {
            subject: "incremental subject",
            recipients: &[],
            body: "body",
            html_body: None,
            message_id: Some(&ids[index]),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let batch = |range: std::ops::Range<usize>| range.map(message).collect::<Vec<_>>();
        let heap_pages = |path: &Path| {
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path).unwrap())).unwrap();
            let inbox = folder_at_path(&store, &["Inbox"]);
            let contents = inbox.contents_table().unwrap();
            assert_eq!(inbox.properties().content_count().unwrap(), 1_700);
            // MS-PST caps every HN allocation, row index nodes and page padding included, at 3580 bytes.
            assert!(contents
                .writer_heap_pages()
                .unwrap()
                .iter()
                .flatten()
                .all(|allocation| allocation.len() <= MAX_HN_ALLOCATION_SIZE));
            let rows = contents.rows_matrix().collect::<Vec<_>>();
            assert_eq!(rows.len(), 1_700);
            for row in &rows {
                assert_eq!(contents.find_row(row.id()).unwrap().id(), row.id());
            }
            for row in [rows[0], rows[799], rows[1_699]] {
                let node = NodeId::from(u32::from(row.id()));
                let message = store
                    .open_message(&store.properties().make_entry_id(node).unwrap(), None)
                    .unwrap();
                assert!(message.properties().get(0x0037).is_some());
            }
            let ipm_id = store.properties().ipm_sub_tree_entry_id().unwrap();
            let ipm = store.open_folder(&ipm_id).unwrap();
            let hierarchy = ipm.hierarchy_table().unwrap();
            let inbox_id = find_child_folder(&store, ipm_id.node_id(), "Inbox")
                .unwrap()
                .unwrap();
            let row = hierarchy
                .find_row(TableRowId::new(u32::from(inbox_id)))
                .unwrap();
            let values = row.columns(hierarchy.context()).unwrap();
            let (column, value) = hierarchy
                .context()
                .columns()
                .iter()
                .zip(&values)
                .find(|(column, _)| column.prop_id() == 0x3602)
                .unwrap();
            assert!(matches!(
                hierarchy.read_column(value.as_ref().unwrap(), column.prop_type()),
                Ok(PropertyValue::Integer32(1_700))
            ));
            contents.writer_heap_pages().unwrap().len()
        };

        // One bulk call versus many small calls: the incremental path crosses row blocks, BTH
        // leaves and a new BTH level, and must not leak heap pages per call.
        let bulk = path("bulk");
        drop(UnicodePstFile::create(&bulk, &message(0)).unwrap());
        drop(UnicodePstFile::append_many(&bulk, &batch(1..1_700)).unwrap());
        let calls = path("calls");
        drop(UnicodePstFile::create(&calls, &message(0)).unwrap());
        for start in (1..1_700).step_by(100) {
            drop(
                UnicodePstFile::append_many(&calls, &batch(start..(start + 100).min(1_700)))
                    .unwrap(),
            );
        }
        let (bulk_pages, call_pages) = (heap_pages(&bulk), heap_pages(&calls));
        // Page-boundary padding costs at most about one BTH leaf per call; leaking the row index
        // per call would add ~2 pages per call here.
        assert!(call_pages <= bulk_pages + 4, "{call_pages} vs {bulk_pages}");
        // Space freed by earlier calls (old B-trees, rewritten table blocks) is reused.
        let size = |path: &Path| std::fs::metadata(path).unwrap().len();
        assert!(
            size(&calls) < size(&bulk) * 5 / 4,
            "{} vs {}",
            size(&calls),
            size(&bulk)
        );
        std::fs::remove_file(bulk).unwrap();
        std::fs::remove_file(calls).unwrap();
    }

    #[test]
    fn fills_nonfinal_data_tree_blocks() {
        let mut blocks = Vec::new();
        let mut next_bid = 1;
        let mut next_offset = FileSpace::from(AMAP_FIRST_OFFSET);
        let root = push_data_tree_chunks(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            [vec![1; 100], vec![2; 50]],
        )
        .unwrap();

        assert!(root.is_internal());
        assert_eq!(blocks[0].size as usize, MAX_DATA_BLOCK_SIZE);
        assert_eq!(blocks[1].size, 50);
        let NewBlockData::DataTree(_, _, total_size) = blocks.last().unwrap().data else {
            panic!("expected XBLOCK root");
        };
        assert_eq!(total_size as usize, MAX_DATA_BLOCK_SIZE + 50);
    }

    #[test]
    fn writes_local_totals_for_multilevel_data_trees() {
        let mut blocks = Vec::new();
        let mut next_bid = 1;
        let mut next_offset = FileSpace::from(AMAP_FIRST_OFFSET);
        let root = push_data_tree_chunks(
            &mut blocks,
            &mut next_bid,
            &mut next_offset,
            (0..1_022).map(|_| vec![0x5A]),
        )
        .unwrap();

        let trees = blocks
            .iter()
            .filter_map(|block| match &block.data {
                NewBlockData::DataTree(entries, level, total_size) => {
                    Some((block.id, *level, entries.len(), *total_size))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let full_group_size = 1_021 * u32::try_from(MAX_DATA_BLOCK_SIZE).unwrap();
        assert_eq!(trees.len(), 3);
        assert_eq!(
            (trees[0].1, trees[0].2, trees[0].3),
            (1, 1_021, full_group_size)
        );
        assert_eq!((trees[1].1, trees[1].2, trees[1].3), (1, 1, 1));
        assert_eq!(
            (trees[2].1, trees[2].2, trees[2].3),
            (2, 2, full_group_size + 1)
        );
        assert_eq!(root, trees[2].0);
    }

    #[test]
    fn aligns_heap_page_maps_to_two_bytes() {
        let data = finish_heap(
            vec![Vec::new(), vec![0x5A]],
            HeapNodeType::Properties,
            heap_id(1).unwrap(),
        )
        .unwrap();
        let mut cursor = Cursor::new(&data);
        let page_map_offset = HeapNodeHeader::read(&mut cursor).unwrap().page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        HeapNodePageMap::read(&mut cursor).unwrap();
        assert_eq!(cursor.position() as usize, data.len());

        let mut heap = HeapBuilder::new();
        heap.alloc(vec![0x5A]).unwrap();
        let pages = heap
            .finish(HeapNodeType::Properties, HeapId::default())
            .unwrap();
        let mut cursor = Cursor::new(&pages[0]);
        let page_map_offset = HeapNodeHeader::read(&mut cursor).unwrap().page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        HeapNodePageMap::read(&mut cursor).unwrap();
        assert_eq!(cursor.position() as usize, pages[0].len());
    }

    /// A non-final heap page is a full data block whose page map follows its last allocation, with zero bytes after it
    /// and no padding allocation, as SCANPST leaves a page it repairs; its stored fill level counts those free bytes
    /// (MS-PST 2.3.1.2, 2.3.1.5).
    fn assert_nonfinal_page_without_padding(pages: &[Vec<u8>], allocated_on_page_0: usize) {
        assert!(pages.len() > 1);
        assert_eq!(pages[0].len(), MAX_DATA_BLOCK_SIZE);
        let mut cursor = Cursor::new(&pages[0]);
        let header = HeapNodeHeader::read(&mut cursor).unwrap();
        let page_map_offset = header.page_map_offset();
        assert_eq!(page_map_offset % 2, 0);
        cursor
            .seek(SeekFrom::Start(u64::from(page_map_offset)))
            .unwrap();
        let page_map = HeapNodePageMap::read(&mut cursor).unwrap();
        // The allocations made, nothing more (the page map does not count the header slot).
        assert_eq!(page_map.allocations().len(), allocated_on_page_0);
        let last = page_map.allocations().last().unwrap();
        assert_eq!(
            usize::from(last.offset() + last.size()).next_multiple_of(2),
            usize::from(page_map_offset)
        );
        let end = cursor.position() as usize;
        assert!(
            pages[0][end..].iter().all(|byte| *byte == 0),
            "bytes after the page map are zero"
        );
        assert_eq!(heap_page_free(&pages[0]), MAX_DATA_BLOCK_SIZE - end);
        assert_eq!(
            pages[0][8] & 0x0F,
            fill_level(MAX_DATA_BLOCK_SIZE - end) as u8,
            "page 0's stored fill level"
        );
    }

    #[test]
    fn a_nonfinal_heap_page_has_no_padding_allocation_when_a_large_one_misses_it() {
        // A maximal allocation that misses the first page by its page map entry leaves 3,584 free bytes on it.
        let mut heap = HeapBuilder::new();
        for _ in 0..2 {
            heap.alloc(vec![0x5A; 2_285]).unwrap();
        }
        heap.alloc(vec![0x5A; MAX_HN_ALLOCATION_SIZE]).unwrap();
        let pages = heap
            .finish(HeapNodeType::Properties, HeapId::default())
            .unwrap();
        assert_eq!(pages.len(), 2);
        assert_nonfinal_page_without_padding(&pages, 2);
    }

    #[test]
    fn a_nonfinal_heap_page_has_no_padding_allocation() {
        let mut heap = HeapBuilder::new();
        let ids = (0..100)
            .map(|_| heap.alloc(vec![0x5A; 100]).unwrap())
            .collect::<Vec<_>>();
        let on_page_0 = ids.iter().filter(|id| id.block_index() == 0).count();
        let pages = heap
            .finish(HeapNodeType::Properties, HeapId::default())
            .unwrap();
        assert_nonfinal_page_without_padding(&pages, on_page_0);
    }

    #[test]
    fn writes_html_with_plain_text_fallback() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-html-{}-{stamp}.pst",
            std::process::id()
        ));
        let html = "<p>HTML 본문</p>".repeat(1_000);
        let input = UnicodePstMessage {
            subject: "HTML message",
            recipients: TEST_RECIPIENTS,
            body: "plain-text fallback",
            html_body: Some(&html),
            message_id: Some("<html@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };

        drop(UnicodePstFile::create(&path, &input).unwrap());
        drop(UnicodePstFile::append(&path, &input).unwrap());
        let message = last_inbox_message(&path);
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), input.body),
            value => panic!("unexpected plain-text body: {value:?}"),
        }
        match message.properties().get(0x1013).unwrap() {
            PropertyValue::Binary(value) => assert_eq!(value.buffer(), html.as_bytes()),
            value => panic!("unexpected HTML body: {value:?}"),
        }
        drop(message);

        let empty_html = UnicodePstMessage {
            html_body: Some(""),
            message_id: Some("<empty-html@example.com>"),
            ..input
        };
        drop(UnicodePstFile::append(&path, &empty_html).unwrap());
        let message = last_inbox_message(&path);
        match message.properties().get(0x1013).unwrap() {
            PropertyValue::Binary(value) => assert!(value.buffer().is_empty()),
            value => panic!("unexpected empty HTML body: {value:?}"),
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn appends_a_message_without_replacing_existing_messages() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-append-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            recipients: TEST_RECIPIENTS,
            body: "first body",
            html_body: None,
            message_id: Some("<first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let second = UnicodePstMessage {
            subject: "second",
            body: "second body",
            message_id: Some("<second@example.com>"),
            ..first
        };
        let third = UnicodePstMessage {
            subject: "third",
            body: "third body",
            message_id: Some("<third@example.com>"),
            ..first
        };

        let created = UnicodePstFile::create(&path, &first).unwrap();
        let old_bbt = *created.header().root().block_btree();
        let old_nbt = *created.header().root().node_btree();
        drop(created);
        let appended = UnicodePstFile::append(&path, &second).unwrap();
        assert_ne!(
            appended.header().root().block_btree().index().index(),
            old_bbt.index().index()
        );
        assert_ne!(
            appended.header().root().node_btree().index().index(),
            old_nbt.index().index()
        );
        let second_bbt = *appended.header().root().block_btree();
        let second_nbt = *appended.header().root().node_btree();
        drop(appended);
        let appended = UnicodePstFile::append(&path, &third).unwrap();
        assert_ne!(
            appended.header().root().block_btree().index().index(),
            second_bbt.index().index()
        );
        assert_ne!(
            appended.header().root().node_btree().index().index(),
            second_nbt.index().index()
        );

        let store = UnicodeStore::read(Rc::new(appended)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3);
        let subjects: Vec<String> = inbox
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                let message_id = NodeId::from(u32::from(row.id()));
                let message = store
                    .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
                    .unwrap();
                assert_eq!(
                    message.recipient_table().unwrap().rows_matrix().count(),
                    TEST_RECIPIENTS.len()
                );
                match message.properties().get(0x0037).unwrap() {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected subject value: {value:?}"),
                }
            })
            .collect();
        assert_eq!(subjects, ["first", "second", "third"]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_appends_messages_and_attachments_in_one_commit() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<bulk-first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        let second = UnicodePstMessage {
            subject: "second",
            message_id: Some("<bulk-second@example.com>"),
            ..first
        };
        let third = UnicodePstMessage {
            subject: "third",
            message_id: Some("<bulk-third@example.com>"),
            ..first
        };
        let attachment = UnicodePstAttachment {
            filename: "bulk.txt",
            mime_type: "text/plain",
            content_id: None,
            data: b"bulk attachment",
            stream: None,
            hidden: false,
            attached_message: None,
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Bulk", "2026"],
                &[
                    UnicodePstBatchMessage {
                        message: second,
                        attachments: &[attachment],
                    },
                    UnicodePstBatchMessage {
                        message: third,
                        attachments: &[],
                    },
                ],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let folder = folder_at_path(&store, &["Bulk", "2026"]);
        assert_eq!(folder.properties().content_count().unwrap(), 2);
        let messages = folder
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                store
                    .open_message(
                        &store
                            .properties()
                            .make_entry_id(NodeId::from(u32::from(row.id())))
                            .unwrap(),
                        None,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .count(),
            1
        );
        assert!(messages[1].attachment_table().is_none());
        drop((messages, folder, store));
        assert_eq!(
            UnicodePstFile::append_many(&path, &[])
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_append_error_reports_position_message_id_and_pst_size() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-error-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<bulk-error-first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        let invalid_attachment = [UnicodePstAttachment {
            filename: "invalid.png",
            mime_type: "image/png",
            content_id: Some("<>"),
            data: &[],
            stream: None,
            hidden: false,
            attached_message: None,
        }];
        let error = UnicodePstFile::append_many_with_attachments(
            &path,
            &[
                UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        message_id: Some("<bulk-error-ok@example.com>"),
                        ..first
                    },
                    attachments: &[],
                },
                UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        message_id: Some("<bulk-error-invalid@example.com>"),
                        ..first
                    },
                    attachments: &invalid_attachment,
                },
            ],
        )
        .err()
        .unwrap();
        let error = error.to_string();
        assert!(error.contains("message 2 of 2"), "{error}");
        assert!(
            error.contains("<bulk-error-invalid@example.com>"),
            "{error}"
        );
        assert!(error.contains("PST size"), "{error}");
        let message = last_inbox_message(&path);
        match message.properties().get(0x0037).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "first"),
            value => panic!("unexpected subject value: {value:?}"),
        }
        drop(message);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bulk_appends_three_thousand_messages_without_cow_explosion() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-bulk-3000-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<bulk-3000-first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());

        let subjects = (0..3_000)
            .map(|index| format!("bulk message {index}"))
            .collect::<Vec<_>>();
        let message_ids = (0..3_000)
            .map(|index| format!("<bulk-{index}@example.com>"))
            .collect::<Vec<_>>();
        let messages = subjects
            .iter()
            .zip(&message_ids)
            .map(|(subject, message_id)| UnicodePstMessage {
                subject,
                message_id: Some(message_id),
                ..first
            })
            .collect::<Vec<_>>();
        let attachment_data = vec![0x5A; 320 * 1_024];
        let attachments = [UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &attachment_data,
            stream: None,
            hidden: false,
            attached_message: None,
        }];
        let batch = messages
            .iter()
            .enumerate()
            .map(|(index, message)| UnicodePstBatchMessage {
                message: *message,
                attachments: if index % 300 == 0 { &attachments } else { &[] },
            })
            .collect::<Vec<_>>();
        drop(UnicodePstFile::append_many_with_attachments(&path, &batch).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 3_001);
        assert_eq!(inbox.contents_table().unwrap().rows_matrix().count(), 3_001);
        assert!(std::fs::metadata(&path).unwrap().len() < 64 * 1024 * 1024);
        drop((inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_and_appends_messages_in_nested_folders() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-folders-{}-{stamp}.pst",
            std::process::id()
        ));
        let first = UnicodePstMessage {
            subject: "first in Projects/2026",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<folder-first@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(
            UnicodePstFile::create_in_folder_with_attachments(
                &path,
                &["Projects", "2026"],
                &first,
                &[UnicodePstAttachment {
                    filename: "folder.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: b"folder attachment",
                    stream: None,
                    hidden: false,
                    attached_message: None,
                }],
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["projects", "2026"],
                &UnicodePstMessage {
                    subject: "second in Projects/2026",
                    message_id: Some("<folder-second@example.com>"),
                    ..first
                },
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["Archive", "2025"],
                &UnicodePstMessage {
                    subject: "in Archive/2025",
                    message_id: Some("<folder-archive@example.com>"),
                    ..first
                },
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append(
                &path,
                &UnicodePstMessage {
                    subject: "still in Inbox",
                    message_id: Some("<folder-inbox@example.com>"),
                    ..first
                },
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let ipm = folder_at_path(&store, &[]);
        // Folder names match exactly, so "projects" is a distinct sibling of "Projects".
        assert_eq!(ipm.hierarchy_table().unwrap().rows_matrix().count(), 5);
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        let lower_projects_2026 = folder_at_path(&store, &["projects", "2026"]);
        assert_eq!(lower_projects_2026.properties().content_count().unwrap(), 1);
        let lower_id = NodeId::from(u32::from(
            lower_projects_2026
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        match store
            .open_message(&store.properties().make_entry_id(lower_id).unwrap(), None)
            .unwrap()
            .properties()
            .get(0x0037)
            .unwrap()
        {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "second in Projects/2026")
            }
            value => panic!("unexpected subject value: {value:?}"),
        }
        drop(lower_projects_2026);
        let projects = folder_at_path(&store, &["Projects"]);
        assert!(projects.properties().has_sub_folders().unwrap());
        assert_eq!(projects.hierarchy_table().unwrap().rows_matrix().count(), 1);
        let projects_2026 = folder_at_path(&store, &["Projects", "2026"]);
        assert_eq!(projects_2026.properties().content_count().unwrap(), 1);
        assert!(projects_2026.associated_table().is_some());
        let subjects = projects_2026
            .contents_table()
            .unwrap()
            .rows_matrix()
            .map(|row| {
                let message = store
                    .open_message(
                        &store
                            .properties()
                            .make_entry_id(NodeId::from(u32::from(row.id())))
                            .unwrap(),
                        None,
                    )
                    .unwrap();
                match message.properties().get(0x0037).unwrap() {
                    PropertyValue::Unicode(value) => value.to_string(),
                    value => panic!("unexpected subject value: {value:?}"),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(subjects, ["first in Projects/2026"]);
        let first_message_id = NodeId::from(u32::from(
            projects_2026
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let first_message = store
            .open_message(
                &store.properties().make_entry_id(first_message_id).unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(
            first_message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .count(),
            1
        );
        let archive_2025 = folder_at_path(&store, &["Archive", "2025"]);
        assert_eq!(archive_2025.properties().content_count().unwrap(), 1);
        assert!(archive_2025.associated_table().is_some());
        drop((
            first_message,
            archive_2025,
            projects_2026,
            projects,
            inbox,
            ipm,
            store,
        ));

        assert_eq!(
            UnicodePstFile::append_in_folder(&path, &[], &first)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_and_appends_large_bodies_and_inline_attachments() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-large-{}-{stamp}.pst",
            std::process::id()
        ));
        let body = "본문".repeat(2_500);
        let attachment_bytes = vec![0xA5; 9_000];
        let input = UnicodePstMessage {
            subject: "large",
            recipients: TEST_RECIPIENTS,
            body: &body,
            html_body: Some("<img src=\"cid:large-image@example.com\">"),
            message_id: Some("<large@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let attachment = UnicodePstAttachment {
            filename: "large.png",
            mime_type: "image/png",
            content_id: Some("<large-image@example.com>"),
            data: &attachment_bytes,
            stream: None,
            hidden: true, // inline (only an inline attachment gets ATT_MHTML_REF)
            attached_message: None,
        };

        drop(UnicodePstFile::create_with_attachments(&path, &input, &[attachment]).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let entry_id = store.properties().make_entry_id(message_id).unwrap();
        let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
        assert_eq!(message.properties().message_flags().unwrap(), 0x11);
        assert!(matches!(
            message.properties().get(0x0E1B),
            Some(PropertyValue::Boolean(true))
        ));
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), body),
            value => panic!("unexpected body value: {value:?}"),
        }
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        // The size counts the stored filename and MIME type bytes, which include no terminators: 9,000 data + 12 (three
        // Integer32) + 3 x 18 ("large.png": file name, long file name, display name) + 8 (".png") + 18 ("image/png") +
        // 46 + 4 (the Content-ID and PidTagAttachFlags) + 1 (PidTagAttachmentHidden's PtypBoolean, as SCANPST requires)
        // = 9,143.
        assert_eq!(attachment.properties().attachment_size().unwrap(), 9_143);
        match attachment.properties().get(0x3001).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "large.png"),
            value => panic!("unexpected attachment display name: {value:?}"),
        }
        match attachment.properties().get(0x3703).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), ".png"),
            value => panic!("unexpected attachment extension: {value:?}"),
        }
        match attachment.properties().get(0x3707).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "large.png"),
            value => panic!("unexpected attachment filename: {value:?}"),
        }
        match attachment.properties().get(0x370E).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), "image/png"),
            value => panic!("unexpected attachment MIME type: {value:?}"),
        }
        match attachment.properties().get(0x3712).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "large-image@example.com")
            }
            value => panic!("unexpected attachment Content-ID: {value:?}"),
        }
        assert!(matches!(
            attachment.properties().get(0x3714),
            Some(PropertyValue::Integer32(0x0000_0004))
        ));
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert_eq!(value.buffer(), attachment_bytes),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        drop(attachment);
        drop(inbox);
        drop(store);
        std::fs::remove_file(&path).unwrap();

        let small_body = "small";
        let first = UnicodePstMessage {
            subject: "first",
            body: small_body,
            message_id: Some("<first@example.com>"),
            ..input
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        drop(
            UnicodePstFile::append_with_attachments(
                &path,
                &input,
                &[UnicodePstAttachment {
                    filename: "large.png",
                    mime_type: "image/png",
                    content_id: Some("large-image@example.com"),
                    data: &attachment_bytes,
                    stream: None,
                    hidden: true,
                    attached_message: None,
                }],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        let message = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_id).unwrap(),
            None,
        )
        .unwrap();
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), body),
            value => panic!("unexpected body value: {value:?}"),
        }
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        match attachment.properties().get(0x3712).unwrap() {
            PropertyValue::Unicode(value) => {
                assert_eq!(value.to_string(), "large-image@example.com")
            }
            value => panic!("unexpected attachment Content-ID: {value:?}"),
        }
        assert!(matches!(
            attachment.properties().get(0x3714),
            Some(PropertyValue::Integer32(0x0000_0004))
        ));
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert_eq!(value.buffer(), attachment_bytes),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        drop(attachment);
        drop(inbox);
        drop(store);
        std::fs::remove_file(path).unwrap();

        assert_eq!(
            attachment_data(
                &UnicodePstAttachment {
                    filename: "invalid.png",
                    mime_type: "image/png",
                    content_id: Some("<>"),
                    data: &[],
                    stream: None,
                    hidden: false,
                    attached_message: None,
                },
                None,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn reserves_allocation_metadata_through_64_gib() {
        const LIMIT: u64 = 64 * 1024 * 1024 * 1024;

        assert_eq!(FPMAP_FIRST_OFFSET, 0x7C004800);
        assert_eq!(map_page_count(FPMAP_FIRST_SIZE), 3);

        for (first, interval) in [
            (AMAP_FIRST_OFFSET, AMAP_DATA_SIZE),
            (PMAP_FIRST_OFFSET, PMAP_DATA_SIZE),
            (FMAP_FIRST_OFFSET, FMAP_DATA_SIZE),
            (FPMAP_FIRST_OFFSET, FPMAP_DATA_SIZE),
        ] {
            let mut metadata_offset = first;
            while metadata_offset < LIMIT {
                let mut next_offset = FileSpace::from(metadata_offset);
                let allocation =
                    reserve_file_range(&mut next_offset, PAGE_SIZE as u64, PAGE_SIZE as u64);
                assert!(
                    allocation > metadata_offset,
                    "allocated over metadata page at 0x{metadata_offset:X}"
                );
                metadata_offset += interval;
            }
        }
    }

    #[test]
    fn supports_multilevel_btrees_and_multiple_amaps() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-multilevel-{}-{stamp}.pst",
            std::process::id()
        ));
        let large_attachment = vec![0x5A; 320_000];
        let first = UnicodePstMessage {
            subject: "message-0",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<message-0@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &first,
                &[UnicodePstAttachment {
                    filename: "large.bin",
                    mime_type: "application/octet-stream",
                    content_id: None,
                    data: &large_attachment,
                    stream: None,
                    hidden: false,
                    attached_message: None,
                }],
            )
            .unwrap(),
        );
        for index in 1..20 {
            let subject = format!("message-{index}");
            let message_id = format!("<message-{index}@example.com>");
            drop(
                UnicodePstFile::append(
                    &path,
                    &UnicodePstMessage {
                        subject: &subject,
                        message_id: Some(&message_id),
                        ..first
                    },
                )
                .unwrap(),
            );
        }

        let pst = UnicodePstFile::open(&path).unwrap();
        assert!(pst.header().root().amap_last_index().index() > AMAP_FIRST_OFFSET);
        let mut reader = pst.reader().lock().unwrap();
        assert!(matches!(
            UnicodeBlockBTree::read(&mut *reader, *pst.header().root().block_btree()).unwrap(),
            RootBTreePage::Intermediate(..)
        ));
        assert!(matches!(
            UnicodeNodeBTree::read(&mut *reader, *pst.header().root().node_btree()).unwrap(),
            RootBTreePage::Intermediate(..)
        ));
        drop(reader);

        let store = UnicodeStore::read(Rc::new(pst)).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 20);
        assert_eq!(inbox.contents_table().unwrap().rows_matrix().count(), 20);
        drop((inbox, store));

        // The allocation map a commit marks from memory equals the spec's B-tree walk rebuild.
        let amap_pages = |path: &Path| {
            let mut file = File::open(path).unwrap();
            let eof = UnicodeHeader::read(&mut file)
                .unwrap()
                .root()
                .file_eof_index()
                .index();
            let mut pages = Vec::new();
            for offset in (AMAP_FIRST_OFFSET..eof).step_by(AMAP_DATA_SIZE as usize) {
                let mut page = [0; PAGE_SIZE];
                file.seek(SeekFrom::Start(offset)).unwrap();
                file.read_exact(&mut page).unwrap();
                pages.push(page);
            }
            pages
        };
        let written = amap_pages(&path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut header = UnicodeHeader::read(&mut file).unwrap();
        header.root_mut().set_amap_status(AmapStatus::Invalid);
        file.seek(SeekFrom::Start(0)).unwrap();
        header.write(&mut file).unwrap();
        drop(file);
        let mut pst = UnicodePstFile::open(&path).unwrap();
        pst.inner.rebuild_allocation_map().unwrap();
        drop(pst);
        assert!(written.len() > 1);
        assert!(
            written == amap_pages(&path),
            "allocation map differs from a rebuild"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn appends_to_an_outlook_created_unicode_pst() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-existing-{}-{stamp}.pst",
            std::process::id()
        ));
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Empty.pst"),
            &path,
        )
        .unwrap();

        // Outlook's fixture uses permute encoding. Decode its data blocks so the same Outlook
        // structure exercises the writer's intentionally unencrypted-only append path.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut header = UnicodeHeader::read(&mut file).unwrap();
        assert_eq!(header.crypt_method(), NdbCryptMethod::Permute);
        let entries = collect_block_btree_entries(&mut file, *header.root().block_btree()).unwrap();
        for entry in entries
            .iter()
            .filter(|entry| !entry.block().block().is_internal())
        {
            file.seek(SeekFrom::Start(entry.block().index().index()))
                .unwrap();
            let block =
                UnicodeDataBlock::read(&mut file, entry.size(), NdbCryptMethod::Permute).unwrap();
            file.seek(SeekFrom::Start(entry.block().index().index()))
                .unwrap();
            UnicodeDataBlock::new(
                NdbCryptMethod::None,
                block.data().to_vec(),
                *block.trailer(),
            )
            .unwrap()
            .write(&mut file)
            .unwrap();
        }
        header.set_crypt_method(NdbCryptMethod::None);
        file.seek(SeekFrom::Start(0)).unwrap();
        header.write(&mut file).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let input = UnicodePstMessage {
            subject: "appended to Outlook PST",
            recipients: TEST_RECIPIENTS,
            body: "existing PST body",
            html_body: None,
            message_id: Some("<existing-pst@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        // A folderless append to the template (whose receive-folder table names the root folder 0x122) is refused and
        // the messages go to a named folder. A writer session rebuilds the template's allocation map when it opens
        // (Outlook's map marks space no B-tree references), so one is opened and closed first; after that the refusal
        // leaves the file byte for byte as it was.
        UnicodePstWriter::open(&path).unwrap().close().unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = UnicodePstFile::append(&path, &input)
            .err()
            .expect("a folderless append to the template is refused");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().contains("outside the IPM subtree"),
            "{error}"
        );
        assert!(
            std::fs::read(&path).unwrap() == before,
            "the refused append changed the file"
        );
        drop(UnicodePstFile::append_in_folder(&path, &["Inbox"], &input).unwrap());
        drop(UnicodePstFile::append_in_folder(&path, &["Inbox"], &input).unwrap());
        drop(UnicodePstFile::append_in_folder(&path, &["Imported", "2026"], &input).unwrap());

        let pst = Rc::new(UnicodePstFile::open(&path).unwrap());
        let next_message = pst
            .header()
            .next_node_index(NodeIdType::NormalMessage)
            .unwrap();
        let mut reader = File::open(&path).unwrap();
        let nbt_entries =
            collect_node_btree_entries(&mut reader, *pst.header().root().node_btree()).unwrap();
        let store = UnicodeStore::read(pst).unwrap();
        let inbox_id = default_receive_folder(&store, &nbt_entries).unwrap();
        let inbox = store
            .open_folder(&store.properties().make_entry_id(inbox_id).unwrap())
            .unwrap();
        assert_eq!(inbox.properties().content_count().unwrap(), 2);
        let contents = inbox.contents_table().unwrap();
        assert!(contents.context().columns().len() > 2);
        let message_ids = contents
            .rows_matrix()
            .map(|row| NodeId::from(u32::from(row.id())))
            .collect::<Vec<_>>();
        assert_eq!(message_ids.len(), 2);
        assert!(message_ids.iter().all(|id| id.index() < next_message));
        for message_id in message_ids {
            let message = store
                .open_message(&store.properties().make_entry_id(message_id).unwrap(), None)
                .unwrap();
            match message.properties().get(0x0037).unwrap() {
                PropertyValue::Unicode(value) => assert_eq!(value.to_string(), input.subject),
                value => panic!("unexpected subject value: {value:?}"),
            }
        }
        let imported = folder_at_path(&store, &["Imported"]);
        assert!(
            imported
                .hierarchy_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        let imported_2026 = folder_at_path(&store, &["Imported", "2026"]);
        assert_eq!(imported_2026.properties().content_count().unwrap(), 1);
        assert!(
            imported_2026
                .contents_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        assert!(
            imported_2026
                .associated_table()
                .unwrap()
                .context()
                .columns()
                .len()
                > 2
        );
        drop((imported_2026, imported, inbox, store));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn externalizes_large_folder_properties() {
        let value = vec![0x5A; MAX_HN_ALLOCATION_SIZE + 1];
        let (pages, external) = property_context_pages(
            [(
                0x300B,
                PropertyValue::Binary(BinaryValue::new(value.clone())),
            )],
            0x300,
        )
        .unwrap();
        assert!(!pages.is_empty());
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].0, NodeId::new(LTP_SUBNODE, 0x300).unwrap());
        assert_eq!(external[0].1, value);
    }

    #[test]
    fn externalizes_unicode_body_at_heap_page_boundary() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-boundary-body-{}-{stamp}.pst",
            std::process::id()
        ));
        let body = "A".repeat(4_081);
        // The stored value has no terminator (with one it would be 8,164 bytes).
        assert_eq!(body.encode_utf16().count() * 2, 8_162);
        assert_eq!(utf16_bytes(&body).len(), 8_162);
        let input = UnicodePstMessage {
            subject: "boundary body",
            recipients: TEST_RECIPIENTS,
            body: &body,
            html_body: None,
            message_id: Some("<boundary-body@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };

        drop(UnicodePstFile::create(&path, &input).unwrap());
        let message = last_inbox_message(&path);
        match message.properties().get(0x1000).unwrap() {
            PropertyValue::Unicode(value) => assert_eq!(value.to_string(), body),
            value => panic!("unexpected body value: {value:?}"),
        }
        drop(message);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_folder_name_past_one_heap_allocation_is_refused() {
        // 1,790 UTF-16 code units (3,580 bytes) is the longest name a new folder's one-page property context holds; one
        // more is refused before anything is written, and the file is not created.
        let longest = "f".repeat(1_790);
        let path = temp_pst("a14-longest-folder-name");
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["Inbox", &longest],
                &edge_message("a14", &[0xA4; 16], true),
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        assert_eq!(
            folder_at_path(&store, &["Inbox", &longest])
                .contents_table()
                .unwrap()
                .rows_matrix()
                .count(),
            1
        );
        std::fs::remove_file(&path).unwrap();
        let too_long = "f".repeat(1_791);
        let Err(error) = UnicodePstFile::create_in_folder(
            &path,
            &["Inbox", &too_long],
            &edge_message("a14", &[0xA4; 16], true),
        ) else {
            panic!("a 1,791-unit folder name was accepted");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("1,790"), "{error}");
        assert!(!path.exists());
        assert!(validate_folder_path(&["Ä".repeat(1_790).as_str()]).is_ok());
        assert!(
            validate_folder_path(&["😀".repeat(896).as_str()]).is_err(),
            "surrogate pairs count as two units"
        );
    }

    #[test]
    fn heaps_put_their_bth_header_and_tcinfo_where_outlook_does() {
        // Every Outlook-written property context has its BTH header at HID 0x20 (hidUserRoot), every table its TCINFO
        // at 0x40 naming its row index's BTH header at 0x20; java-libpst and pst-extractor read only those.
        fn allocation(page: &[u8], index: usize) -> &[u8] {
            let map = usize::from(u16::from_le_bytes([page[0], page[1]]));
            let at = |k: usize| {
                usize::from(u16::from_le_bytes([
                    page[map + 4 + 2 * k],
                    page[map + 5 + 2 * k],
                ]))
            };
            &page[at(index - 1)..at(index)]
        }
        let user_root = |page: &[u8]| u32::from_le_bytes(page[4..8].try_into().unwrap());
        let subject = PropertyValue::Unicode(UnicodeValue::new("subject".encode_utf16().collect()));
        let (pages, _) = property_context_pages([(0x0037, subject)], 0x80).unwrap();
        assert_eq!(
            (user_root(&pages[0]), allocation(&pages[0], 1)[0]),
            (0x20, 0xB5)
        );
        let message = NodeId::new(NodeIdType::NormalMessage, 0x200).unwrap();
        let tables = [
            table_context_data(&[]).unwrap(),
            table_context_data(&[message]).unwrap(),
            recipient_table_data(TEST_RECIPIENTS, 1)
                .unwrap()
                .0
                .remove(0),
            attachment_table_data(&[], &[], &[], 1).unwrap().0.remove(0),
        ];
        for (k, page) in tables.iter().enumerate() {
            let tcinfo = allocation(page, 2);
            let row_index = u32::from_le_bytes(tcinfo[10..14].try_into().unwrap());
            assert_eq!(
                (
                    user_root(page),
                    tcinfo[0],
                    row_index,
                    allocation(page, 1)[0]
                ),
                (0x40, 0x7C, 0x20, 0xB5),
                "table {k}"
            );
        }
    }

    #[test]
    fn attachment_values_over_3580_bytes_are_subnodes_of_the_attachment() {
        // MS-PST 2.3.1 caps a heap allocation at 3,580 bytes and 2.3.3.3 puts a larger property value in a subnode. A
        // 1,900-character file name (3,800 bytes) is written three times (PidTagDisplayName, PidTagAttachFilename,
        // PidTagAttachLongFilename): three subnodes, and no heap page over a block. One heap page holding every value
        // would have 3,800-byte allocations and a page past the block limit ("Invalid BLOCKTRAILER cb"). The same for
        // an attached message's display name.
        let long = "n".repeat(1_900);
        let file = UnicodePstAttachment {
            filename: &long,
            mime_type: "text/plain",
            content_id: None,
            data: b"x",
            stream: None,
            hidden: false,
            attached_message: None,
        };
        let data_node = NodeId::new(LTP_SUBNODE, 0x200).unwrap();
        let (pages, external) = attachment_data(&file, Some(data_node)).unwrap();
        assert!(
            pages.iter().all(|page| page.len() <= MAX_DATA_BLOCK_SIZE),
            "a page over a block"
        );
        assert_eq!(
            external
                .iter()
                .map(|(_, data)| data.len())
                .collect::<Vec<_>>(),
            [3_800; 3]
        );
        assert!(
            external.iter().all(|(node, _)| *node != data_node),
            "a value subnode meets the data subnode"
        );
        let inner = edge_message("m27-inner", &[0xE7; 16], true);
        let message = UnicodePstAttachment {
            filename: &long,
            mime_type: "",
            content_id: None,
            data: &[],
            stream: None,
            hidden: false,
            attached_message: Some(Box::new(UnicodePstAttachedMessage {
                message: inner,
                attachments: Vec::new(),
            })),
        };
        let node = NodeId::new(NodeIdType::NormalMessage, 0x200).unwrap();
        let (_, external) = attached_message_data(&message, node, 100, 113).unwrap();
        assert_eq!(
            external
                .iter()
                .map(|(_, data)| data.len())
                .collect::<Vec<_>>(),
            [3_800]
        );

        // Written and read back whole through this crate's reader; the test build checks the file after the commit.
        let path = temp_pst("m27-long-names");
        let attachments = [file, message];
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &edge_message("m27", &[0xE8; 16], true),
                &attachments,
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let row = contents.rows_matrix().next().unwrap();
        let message = UnicodeMessage::read(
            store.clone(),
            &store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap(),
            None,
        )
        .unwrap();
        let rows = message
            .attachment_table()
            .unwrap()
            .rows_matrix()
            .map(|row| NodeId::from(u32::from(row.id())))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        let file = UnicodeAttachment::read(message.clone(), rows[0], None).unwrap();
        for id in [0x3001, 0x3704, 0x3707] {
            assert!(
                matches!(file.properties().get(id), Some(PropertyValue::Unicode(name)) if name.to_string() == long),
                "file {id:#06x}"
            );
        }
        assert!(
            matches!(file.data(), Some(AttachmentData::Binary(value)) if value.buffer() == b"x")
        );
        let attached = UnicodeAttachment::read(message.clone(), rows[1], None).unwrap();
        assert!(
            matches!(attached.properties().get(0x3001), Some(PropertyValue::Unicode(name)) if name.to_string() == long)
        );
        assert!(matches!(attached.data(), Some(AttachmentData::Message(_))));
        // PidTagMessageSize counts the value subnodes: the same message with one-character names is smaller by at least
        // the four 3,800-byte values (less the short names' four 2-byte values and the extension the long name lacks).
        let Some(PropertyValue::Integer32(long_size)) = message.properties().get(0x0E08).cloned()
        else {
            panic!("size")
        };
        let short_path = temp_pst("m27-short-names");
        let short_attachments = [
            UnicodePstAttachment {
                filename: "n",
                mime_type: "text/plain",
                content_id: None,
                data: b"x",
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "n",
                mime_type: "",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: Some(Box::new(UnicodePstAttachedMessage {
                    message: edge_message("m27-inner", &[0xE7; 16], true),
                    attachments: Vec::new(),
                })),
            },
        ];
        drop(
            UnicodePstFile::create_with_attachments(
                &short_path,
                &edge_message("m27", &[0xE8; 16], true),
                &short_attachments,
            )
            .unwrap(),
        );
        let store =
            UnicodeStore::read(Rc::new(UnicodePstFile::open(&short_path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let row = contents.rows_matrix().next().unwrap();
        let short = UnicodeMessage::read(
            store.clone(),
            &store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap(),
            None,
        )
        .unwrap();
        let Some(PropertyValue::Integer32(short_size)) = short.properties().get(0x0E08).cloned()
        else {
            panic!("size")
        };
        assert!(
            long_size - short_size >= 4 * (3_800 - 2),
            "PidTagMessageSize {long_size} against {short_size}"
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(short_path).unwrap();
    }

    #[test]
    fn a_file_attachment_s_extension_follows_ms_oxcmail_2_2_3_4_2() {
        // The part after the last period, leading and trailing spaces and periods removed; none when it is empty. The
        // name keeps 2.2.3.4.2, the section's number in an earlier MS-OXCMAIL version; it is now 2.2.3.4.1.1.
        for (filename, extension) in [
            ("report.pdf", Some(".pdf")),
            ("a.tar.gz", Some(".gz")),
            ("Türkçe dosya adı.txt", Some(".txt")),
            ("report.pdf  ", Some(".pdf")),
            (".bashrc", Some(".bashrc")),
            ("no-period", None),
            ("trailing.", None),
            ("dots...", None),
            ("name. ", None),
            // MS-OXCMAIL 2.2.3.4.1.1 step 1, Unicode separators as spaces.
            ("report.pdf\u{A0}", Some(".pdf")),
            ("report.p\u{3000}df", Some(".p df")),
            ("name.\u{2028}", None),
            ("name.\u{180E}x", Some(".x")),
        ] {
            assert_eq!(
                attachment_extension(filename).as_deref(),
                extension,
                "{filename:?}"
            );
        }
    }

    #[test]
    fn writes_an_empty_attachment() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-empty-attachment-{}-{stamp}.pst",
            std::process::id()
        ));
        let message = UnicodePstMessage {
            subject: "empty attachment",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some("<empty-attachment@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &message,
                &[UnicodePstAttachment {
                    filename: "empty.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: &[],
                    stream: None,
                    hidden: false,
                    attached_message: None,
                }],
            )
            .unwrap(),
        );
        drop(
            UnicodePstFile::append_with_attachments(
                &path,
                &message,
                &[UnicodePstAttachment {
                    filename: "empty.txt",
                    mime_type: "text/plain",
                    content_id: None,
                    data: &[],
                    stream: None,
                    hidden: false,
                    attached_message: None,
                }],
            )
            .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .last()
                .unwrap()
                .id(),
        ));
        let message = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_id).unwrap(),
            None,
        )
        .unwrap();
        let attachment_id = NodeId::from(u32::from(
            message
                .attachment_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let attachment = UnicodeAttachment::read(message, attachment_id, None).unwrap();
        // As above: 12 + 3 x 18 ("empty.txt") + 8 (".txt") + 20 ("text/plain") + 1 (PidTagAttachmentHidden) = 95.
        assert_eq!(attachment.properties().attachment_size().unwrap(), 95);
        match attachment.data().unwrap() {
            AttachmentData::Binary(value) => assert!(value.buffer().is_empty()),
            AttachmentData::Message(_) => panic!("unexpected embedded message"),
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_attachment_size_is_the_sum_of_its_stored_values_as_scanpst_requires() {
        // SCANPST repairs a file attachment's PidTagAttachSize to the sum of its property values' sizes (PtypBoolean
        // one byte, strings their UTF-16 bytes, the data its length, PidTagAttachSize itself four), which is also what
        // Outlook's own attachments carry. The sum here is taken from the values read back, so a property added later
        // without being counted fails it.
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-attach-size-{}-{stamp}.pst",
            std::process::id()
        ));
        let message = UnicodePstMessage {
            subject: "attachment sizes",
            recipients: TEST_RECIPIENTS,
            body: "body",
            html_body: Some("<img src=\"cid:pic@example.com\">"),
            message_id: Some("<attachment-sizes@example.com>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let long_name = format!("{}.bin", "n".repeat(1_900));
        let picture = vec![0x5A; 9_000];
        let attachments = [
            UnicodePstAttachment {
                filename: "pic.png",
                mime_type: "image/png",
                content_id: Some("<pic@example.com>"),
                data: &picture,
                stream: None,
                hidden: true,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "empty.txt",
                mime_type: "text/plain",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: &long_name,
                mime_type: "application/octet-stream",
                content_id: None,
                data: b"long name",
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "README",
                mime_type: "text/plain",
                content_id: None,
                data: b"no extension",
                stream: None,
                hidden: false,
                attached_message: None,
            },
        ];
        drop(UnicodePstFile::create_with_attachments(&path, &message, &attachments).unwrap());

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let inbox = folder_at_path(&store, &["Inbox"]);
        let message_id = NodeId::from(u32::from(
            inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id(),
        ));
        let message = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_id).unwrap(),
            None,
        )
        .unwrap();
        let rows = message
            .attachment_table()
            .unwrap()
            .rows_matrix()
            .map(|row| NodeId::from(u32::from(row.id())))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), attachments.len());
        for id in rows {
            let attachment = UnicodeAttachment::read(message.clone(), id, None).unwrap();
            let data = match attachment.data().unwrap() {
                AttachmentData::Binary(value) => value.buffer().len(),
                AttachmentData::Message(_) => panic!("unexpected embedded message"),
            };
            let values: usize = attachment
                .properties()
                .iter()
                .filter(|(id, _)| **id != 0x3701)
                .map(|(id, value)| match value {
                    PropertyValue::Boolean(_) => 1,
                    PropertyValue::Integer32(_) => 4,
                    PropertyValue::Unicode(value) => value.buffer().len() * 2,
                    PropertyValue::Binary(value) => value.buffer().len(),
                    value => panic!("unexpected attachment property {id:#06X}: {value:?}"),
                })
                .sum();
            assert_eq!(
                attachment.properties().attachment_size().unwrap() as usize,
                values + data,
                "attachment {id:?}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    // ---- Library edges on hand-modified ("foreign") files ----

    #[test]
    fn message_sizes_keep_their_values_and_match_the_contents_table() {
        // PidTagMessageSize is set in place once a message's property context is built. The expected values are fixed,
        // so a change of the formula or of the value set fails here. Every message's property also equals its
        // contents-table cell, and no message carries the two row identifiers (PidTagLtpRowId and PidTagLtpRowVer, two
        // eight-byte BTH records, are not in a message's property context).
        let path = temp_pst("a3-sizes");
        let keys = [
            [0xA3_u8; 16],
            [0xA4; 16],
            [0xA5; 16],
            [0xA6; 16],
            [0xA7; 16],
        ];
        let names = (0..200)
            .map(|index| {
                (
                    format!("Recipient {index:03}"),
                    format!("r{index:03}@example.com"),
                )
            })
            .collect::<Vec<_>>();
        let many = names
            .iter()
            .map(|(name, email)| UnicodePstRecipient {
                name,
                email,
                recipient_type: UnicodePstRecipientType::To,
                properties: &[],
            })
            .collect::<Vec<_>>();
        let body = "large body ".repeat(500);
        let html = format!("<html><body>{body}</body></html>");
        let content = vec![7_u8; 20_000];
        let attachments = [
            UnicodePstAttachment {
                filename: "inline.png",
                mime_type: "image/png",
                content_id: Some("<a3@example.com>"),
                data: b"png",
                stream: None,
                hidden: true,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "empty.txt",
                mime_type: "text/plain",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "data.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &content,
                stream: None,
                hidden: false,
                attached_message: None,
            },
        ];
        let categories = ["A3", "Größe"];
        let plain = edge_message("a3-plain", &keys[0], true);
        let batch = [
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    recipients: TEST_RECIPIENTS,
                    ..edge_message("a3-attachments", &keys[1], false)
                },
                attachments: &attachments,
            },
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    body: &body,
                    html_body: Some(&html),
                    ..edge_message("a3-large-body", &keys[2], true)
                },
                attachments: &[],
            },
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    recipients: &many,
                    ..edge_message("a3-recipients", &keys[3], true)
                },
                attachments: &[],
            },
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    categories: &categories,
                    ..edge_message("a3-categories", &keys[4], false)
                },
                attachments: &[],
            },
        ];
        drop(UnicodePstFile::create(&path, &plain).unwrap());
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let column = contents
            .context()
            .columns()
            .iter()
            .position(|column| column.prop_id() == 0x0E08)
            .expect("a PidTagMessageSize column");
        let mut sizes = Vec::new();
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let subject = match message.properties().get(0x0037) {
                Some(PropertyValue::Unicode(value)) => value.to_string(),
                other => panic!("subject: {other:?}"),
            };
            let Some(PropertyValue::Integer32(size)) = message.properties().get(0x0E08).cloned()
            else {
                panic!("{subject}: no PidTagMessageSize")
            };
            assert!(
                message.properties().get(0x67F2).is_none()
                    && message.properties().get(0x67F3).is_none(),
                "{subject}: row identifiers in the message"
            );
            let values = row.columns(contents.context()).unwrap();
            let cell = contents
                .read_column(values[column].as_ref().unwrap(), PropertyType::Integer32)
                .unwrap();
            assert!(
                matches!(cell, PropertyValue::Integer32(value) if value == size),
                "{subject}: contents cell {cell:?}, message {size}"
            );
            sizes.push((subject, size));
        }
        sizes.sort();
        println!("{sizes:?}");
        // The expected values count every byte the message's node and subnodes hold, as Outlook stores it and as
        // `session::tests::message_size_is_every_byte_its_node_and_subnodes_hold` measures; a message without
        // attachments has no attachment table, so none is counted. a3-attachments: its three file attachments carry
        // PidTagDisplayName and PidTagAttachExtension, values 20 + 8 ("inline.png", ".png") + 18 + 8 ("empty.txt",
        // ".txt") + 16 + 8 ("data.bin", ".bin") = 78, with six BTH records of 8 bytes = 48 and six heap allocations'
        // page-map entries of 2 bytes = 12. a3-recipients: its recipient table's row-index header and TCINFO come first
        // (HID 0x20, 0x40), so page 0 of that 7-page heap holds 220 allocations; non-final pages are padded to the full
        // 8,176 bytes, and the final page holds 73 allocations and 4,232 bytes.
        let expected = [
            ("a3-attachments", 22622),
            ("a3-categories", 690),
            ("a3-large-body", 17178),
            ("a3-plain", 624),
            ("a3-recipients", 70974),
        ];
        assert_eq!(
            sizes,
            expected
                .iter()
                .map(|(subject, size)| (subject.to_string(), *size))
                .collect::<Vec<_>>()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn transport_headers_are_written_as_given_and_only_when_given() {
        // PidTagTransportMessageHeaders (0x007D, PtypString) carries the input's text exactly, short (in the heap),
        // long (past the heap's 3,580-byte allocation limit, so in a subnode), with CRLF and non-ASCII text, and empty;
        // a message without headers has no such property. The message size counts them.
        let path = temp_pst("c3-headers");
        let keys = [[0xC3_u8; 16], [0xC4; 16], [0xC5; 16], [0xC6; 16]];
        let long = "Received: from relay.example.com by mx.example.com; Thu, 29 Oct 2026 09:00:00 +0300\r\n".repeat(200);
        let cases: [(&str, Option<&str>); 4] = [
            ("c3-none", None),
            (
                "c3-short",
                Some("Received: from a.example.com\r\nX-Türkçe: ğüşöçı 日本語\r\n"),
            ),
            ("c3-long", Some(&long)),
            ("c3-empty", Some("")),
        ];
        drop(
            UnicodePstFile::create(
                &path,
                &UnicodePstMessage {
                    transport_headers: cases[0].1,
                    ..edge_message(cases[0].0, &keys[0], true)
                },
            )
            .unwrap(),
        );
        let batch = cases[1..]
            .iter()
            .zip(&keys[1..])
            .map(|((subject, headers), key)| UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    transport_headers: *headers,
                    ..edge_message(subject, key, true)
                },
                attachments: &[],
            })
            .collect::<Vec<_>>();
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut seen = Vec::new();
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let Some(PropertyValue::Unicode(subject)) = message.properties().get(0x0037) else {
                panic!("subject")
            };
            let subject = subject.to_string();
            let (_, wanted) = cases.iter().find(|(name, _)| *name == subject).unwrap();
            match (message.properties().get(0x007D), wanted) {
                (None, None) => {}
                // A present empty value is HNID 0, which this crate's reader returns as Null; absent stays None.
                (Some(PropertyValue::Null), Some("")) => {}
                (Some(PropertyValue::Unicode(value)), Some(text)) => {
                    assert_eq!(value.to_string(), *text, "{subject}")
                }
                (other, _) => {
                    panic!("{subject}: PidTagTransportMessageHeaders {other:?}, wanted {wanted:?}")
                }
            }
            let Some(PropertyValue::Integer32(size)) = message.properties().get(0x0E08).cloned()
            else {
                panic!("{subject}: no size")
            };
            seen.push((subject, size));
        }
        seen.sort();
        let size = |name: &str| seen.iter().find(|(subject, _)| subject == name).unwrap().1;
        assert_eq!(seen.len(), 4);
        // The same message with headers is larger by at least the headers' UTF-16 bytes.
        assert!(
            size("c3-long") - size("c3-none") >= 2 * long.len() as i32,
            "{seen:?}"
        );
        assert!(
            size("c3-short") > size("c3-empty") && size("c3-empty") >= size("c3-none"),
            "{seen:?}"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_folder_path_is_made_to_exist_empty_exactly_and_only_once() {
        // `ensure_folder_path` creates every missing folder of a path, with no message in it (counts 0, an empty
        // contents table) and the parents' has-subfolders flags set; names are exact, so a case variant is another
        // folder; an existing path writes nothing. The test build checks every commit's file state.
        let path = temp_pst("c4-folders");
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        let message = edge_message("c4-item", &[0xC4; 16], true);
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap();
        for folder_path in [
            &["Archive", "2025", "Q1"][..],
            &["Empty top"],
            &["Inbox", "Empty child"],
            &["archive"],
        ] {
            writer.ensure_folder_path(folder_path).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        for folder_path in [
            &["Archive"][..],
            &["Archive", "2025", "Q1"],
            &["Inbox"],
            &["Deleted Items"],
        ] {
            writer.ensure_folder_path(folder_path).unwrap();
        }
        assert!(
            std::fs::read(&path).unwrap() == before,
            "an existing path wrote something"
        );
        writer.close().unwrap();

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        for (folder_path, has_children) in [
            (&["Archive"][..], true),
            (&["Archive", "2025"], true),
            (&["Archive", "2025", "Q1"], false),
            (&["Empty top"], false),
            (&["Inbox", "Empty child"], false),
            (&["archive"], false),
        ] {
            let folder = folder_at_path(&store, folder_path);
            assert_eq!(
                folder.properties().display_name().unwrap(),
                *folder_path.last().unwrap()
            );
            assert_eq!(
                folder.properties().content_count().unwrap(),
                0,
                "{folder_path:?}"
            );
            assert_eq!(
                folder.properties().unread_count().unwrap(),
                0,
                "{folder_path:?}"
            );
            assert_eq!(
                folder.properties().has_sub_folders().unwrap(),
                has_children,
                "{folder_path:?}"
            );
            assert_eq!(
                folder
                    .contents_table()
                    .map_or(0, |table| table.rows_matrix().count()),
                0,
                "{folder_path:?}"
            );
        }
        let inbox = folder_at_path(&store, &["Inbox"]);
        assert_eq!(inbox.properties().content_count().unwrap(), 1);
        assert!(inbox.properties().has_sub_folders().unwrap());
        assert!(std::fs::metadata(&path).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    /// A chain of attached e-mails `depth` levels deep, each with recipients, a body and a file; the innermost has only
    /// the file. Its strings are leaked: test data only.
    #[test]
    fn a_message_is_written_with_its_class_flags_and_times_and_without_what_it_does_not_have() {
        // The message class is written as given, in the message and in its contents-table row; a message without a
        // sender, Message-ID or delivery time has none of those properties, in the message or in the row; mfUnsent
        // marks a draft; the creation and last-modification times are the input's. A complete message next to it keeps
        // every property. An attached message follows the same rules. The test build checks every commit.
        let path = temp_pst("m19-draft");
        let complete = UnicodePstMessage {
            message_class: "ipm.note",
            creation_time: 133_750_000_000_000_000,
            last_modification_time: 133_750_090_000_000_000,
            ..edge_message("m19-complete", &[0xD1; 16], true)
        };
        let absent = |subject: &'static str, key: &'static [u8; 16]| UnicodePstMessage {
            sender: None,
            message_id: None,
            delivery_time: None,
            unsent: true,
            named_properties: &[],
            properties: &[],
            message_class: "IPM.Note.Custom draft",
            creation_time: 133_750_010_000_000_000,
            last_modification_time: 133_750_020_000_000_000,
            ..edge_message(subject, key, false)
        };
        let attachments = [UnicodePstAttachment {
            filename: "Draft attached",
            mime_type: "",
            content_id: None,
            data: &[],
            stream: None,
            hidden: false,
            attached_message: Some(Box::new(UnicodePstAttachedMessage {
                message: absent("m19-inner", &[0xD3; 16]),
                attachments: vec![],
            })),
        }];
        drop(UnicodePstFile::create(&path, &complete).unwrap());
        let batch = [UnicodePstBatchMessage {
            message: absent("m19-draft", &[0xD2; 16]),
            attachments: &attachments,
        }];
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Drafts"], &batch)
                .unwrap(),
        );

        let sender_ids = [0x0042_u16, 0x0064, 0x0065, 0x0C1A, 0x0C1E, 0x0C1F];
        let check = |message: &UnicodeMessage, subject: &str, draft: bool| {
            let properties = message.properties();
            let class = if draft {
                "IPM.Note.Custom draft"
            } else {
                "ipm.note"
            };
            assert!(
                matches!(properties.get(0x001A), Some(PropertyValue::Unicode(value)) if value.to_string() == class),
                "{subject}: class"
            );
            let Some(PropertyValue::Integer32(flags)) = properties.get(0x0E07).cloned() else {
                panic!("{subject}: flags")
            };
            assert_eq!(
                flags & 0x08 != 0,
                draft,
                "{subject}: mfUnsent in {flags:#x}"
            );
            let (created, modified) = if draft {
                (133_750_010_000_000_000, 133_750_020_000_000_000)
            } else {
                (133_750_000_000_000_000, 133_750_090_000_000_000)
            };
            assert!(
                matches!(properties.get(0x3007), Some(PropertyValue::Time(value)) if *value == created),
                "{subject}: creation time"
            );
            assert!(
                matches!(properties.get(0x3008), Some(PropertyValue::Time(value)) if *value == modified),
                "{subject}: modification time"
            );
            for id in sender_ids.iter().copied().chain([0x0E06, 0x1035]) {
                assert_eq!(
                    properties.get(id).is_none(),
                    draft,
                    "{subject}: property {id:#06x}"
                );
            }
        };
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        for (folder, subject, draft) in [
            (&["Inbox"][..], "m19-complete", false),
            (&["Drafts"], "m19-draft", true),
        ] {
            let contents = folder_at_path(&store, folder)
                .contents_table()
                .unwrap()
                .clone();
            let rows = contents.rows_matrix().collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "{subject}");
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(rows[0].id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            assert!(
                matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(value)) if value.to_string() == subject)
            );
            check(&message, subject, draft);
            // The contents-table row: the class and the modification time as given; no cell for what is absent.
            let values = rows[0].columns(contents.context()).unwrap();
            let cell = |id: u16| {
                let column = contents
                    .context()
                    .columns()
                    .iter()
                    .position(|column| column.prop_id() == id)
                    .unwrap_or_else(|| panic!("no column {id:#06x}"));
                values[column].as_ref().map(|value| {
                    contents
                        .read_column(value, contents.context().columns()[column].prop_type())
                        .unwrap()
                })
            };
            let class = if draft {
                "IPM.Note.Custom draft"
            } else {
                "ipm.note"
            };
            assert!(
                matches!(cell(0x001A), Some(PropertyValue::Unicode(value)) if value.to_string() == class),
                "{subject}: class cell"
            );
            let modified = if draft {
                133_750_020_000_000_000
            } else {
                133_750_090_000_000_000
            };
            assert!(
                matches!(cell(0x3008), Some(PropertyValue::Time(value)) if value == modified),
                "{subject}: modification cell"
            );
            let Some(PropertyValue::Integer32(flags)) = cell(0x0E07) else {
                panic!("{subject}: flags cell")
            };
            assert_eq!(flags & 0x08 != 0, draft, "{subject}: mfUnsent in the row");
            for id in [0x0042_u16, 0x0C1A, 0x0E06, 0x1035] {
                if contents
                    .context()
                    .columns()
                    .iter()
                    .any(|column| column.prop_id() == id)
                {
                    assert_eq!(cell(id).is_none(), draft, "{subject}: cell {id:#06x}");
                }
            }
            if draft {
                let table = message
                    .attachment_table()
                    .expect("the draft's attachment table");
                let row = table.rows_matrix().next().expect("one attachment");
                let attachment = UnicodeAttachment::read(
                    message.clone(),
                    NodeId::from(u32::from(row.id())),
                    None,
                )
                .unwrap();
                let inner = attachment.attached_message().expect("the attached message");
                assert!(
                    matches!(inner.properties().get(0x0037), Some(PropertyValue::Unicode(value)) if value.to_string() == "m19-inner")
                );
                check(&inner, "m19-inner", true);
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_message_class_outside_the_specification_or_a_message_without_any_key_is_refused() {
        // MS-OXCMSG 2.2.1.3 allows 1 to 255 ASCII characters from 0x20 to 0x7F, not ending with a period. Anything else
        // is invalid input, refused before anything is written, at any depth. So is a message with neither a search key
        // nor a Message-ID to derive one from.
        let path = temp_pst("m19-class");
        drop(UnicodePstFile::create(&path, &edge_message("m19-first", &[0xE1; 16], true)).unwrap());
        let before = std::fs::read(&path).unwrap();
        let long = "A".repeat(256);
        let longest = "A".repeat(255);
        for class in [
            "",
            "IPM.Note.",
            "IPM.N\u{f6}te",
            "IPM.Note\t",
            "IPM.Note\u{1f}",
            long.as_str(),
        ] {
            let input = UnicodePstMessage {
                message_class: class,
                ..edge_message("m19-bad", &[0xE2; 16], true)
            };
            let error = UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: input,
                    attachments: &[],
                }],
            )
            .map(drop)
            .unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "{class:?}: {error}"
            );
            let attachments = [UnicodePstAttachment {
                filename: "inner",
                mime_type: "",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: Some(Box::new(UnicodePstAttachedMessage {
                    message: input,
                    attachments: vec![],
                })),
            }];
            let outer = edge_message("m19-outer", &[0xE3; 16], true);
            let error = UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: outer,
                    attachments: &attachments,
                }],
            )
            .map(drop)
            .unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "attached {class:?}: {error}"
            );
        }
        let keyless = UnicodePstMessage {
            search_key: None,
            message_id: None,
            ..edge_message("m19-keyless", &[0xE4; 16], true)
        };
        let error = UnicodePstFile::append_many_in_folder_with_attachments(
            &path,
            &["Inbox"],
            &[UnicodePstBatchMessage {
                message: keyless,
                attachments: &[],
            }],
        )
        .map(drop)
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert!(
            std::fs::read(&path).unwrap() == before,
            "a refused message wrote something"
        );
        // The limits themselves are accepted: 255 characters, and 0x20 and 0x7F inside ("0x20 through 0x7F").
        for class in [longest.as_str(), "IPM.Note.With space~", "IPM.Note\u{7f}"] {
            let input = UnicodePstMessage {
                message_class: class,
                ..edge_message("m19-good", &[0xE5; 16], true)
            };
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: input,
                    attachments: &[],
                }],
            )
            .unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }

    fn given_value(value: &PropertyValue) -> String {
        match value {
            PropertyValue::Integer32(value) => format!("i{value}"),
            PropertyValue::Boolean(value) => format!("b{value}"),
            PropertyValue::Time(value) => format!("t{value}"),
            // A present, empty string or binary is HNID 0; this crate's reader gives an empty string as Null.
            PropertyValue::Null => "empty".to_owned(),
            PropertyValue::Unicode(value) if value.to_string().is_empty() => "empty".to_owned(),
            PropertyValue::Unicode(value) => format!("s{value}"),
            PropertyValue::Binary(value) if value.buffer().is_empty() => "empty".to_owned(),
            PropertyValue::Binary(value) => format!("x{:02x?}", value.buffer()),
            PropertyValue::MultipleUnicode(values) => format!(
                "m{:?}",
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            ),
            other => format!("?{other:?}"),
        }
    }

    fn wanted(value: &UnicodePstValue<'_>) -> String {
        match value {
            UnicodePstValue::Integer32(value) => format!("i{value}"),
            UnicodePstValue::Boolean(value) => format!("b{value}"),
            UnicodePstValue::Time(value) => format!("t{value}"),
            UnicodePstValue::Unicode("") | UnicodePstValue::Binary(&[]) => "empty".to_owned(),
            UnicodePstValue::Unicode(value) => format!("s{value}"),
            UnicodePstValue::Binary(value) => format!("x{:02x?}", value),
            UnicodePstValue::MultipleUnicode(values) => format!(
                "m{:?}",
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            ),
        }
    }

    /// The cells of a row cover every byte from 0 to the end of the 1-byte cells, once, as in every table Outlook
    /// writes; a gap is what SCANPST reports as "TCINFO COLDESCs do not account for all the space in the row", and its
    /// repair then rebuilds the recipient table without its rows.
    fn assert_row_without_gaps(context: &TableContextInfo, label: &str) {
        let mut covered = vec![0_u8; usize::from(context.end_1byte_values())];
        for column in context.columns() {
            for byte in column.offset()..column.offset() + u16::from(column.size()) {
                covered[usize::from(byte)] += 1;
            }
        }
        let layout = context
            .columns()
            .iter()
            .map(|column| (column.prop_id(), column.offset(), column.size()))
            .collect::<Vec<_>>();
        assert!(
            covered.iter().all(|count| *count == 1),
            "{label}: row bytes not covered exactly once: {covered:?} {layout:04x?}"
        );
        assert_eq!(
            context.end_4byte_values(),
            context.end_2byte_values(),
            "{label}"
        );
    }

    #[test]
    fn recipient_table_rows_have_no_gap_whatever_the_given_cells() {
        // Two given 4-byte cells (a meeting request's PidTagRecipientTrackStatus and PidTagRecipientFlags), and two
        // strings before two times (a delivery report), would leave bytes 60-63 of the row to no column if the 8-byte
        // cells were put at an 8-byte boundary, which Outlook's own tables do not do; SCANPST then finds "Errors" and
        // its repair deletes the recipients.
        let path = temp_pst("m31-recipient-gap");
        let meeting = [
            UnicodePstProperty {
                id: 0x5FFD,
                value: UnicodePstValue::Integer32(1),
            },
            UnicodePstProperty {
                id: 0x5FFF,
                value: UnicodePstValue::Integer32(0),
            },
        ];
        let report = [
            UnicodePstProperty {
                id: 0x0010,
                value: UnicodePstValue::Time(133_485_768_000_000_000),
            },
            UnicodePstProperty {
                id: 0x0032,
                value: UnicodePstValue::Time(133_485_768_000_000_000),
            },
            UnicodePstProperty {
                id: 0x0C1B,
                value: UnicodePstValue::Unicode("<remote.example.invalid #2.0.0>"),
            },
            UnicodePstProperty {
                id: 0x0C21,
                value: UnicodePstValue::Unicode("remote.example.invalid"),
            },
        ];
        let one_time = [UnicodePstProperty {
            id: 0x0032,
            value: UnicodePstValue::Time(133_485_768_000_000_000),
        }];
        let cases: [(&str, &[UnicodePstProperty<'_>]); 3] = [
            ("meeting", &meeting),
            ("report", &report),
            ("one time", &one_time),
        ];
        let recipients = cases.map(|(_, properties)| {
            [UnicodePstRecipient {
                name: "Given",
                email: "given@example.invalid",
                recipient_type: UnicodePstRecipientType::To,
                properties,
            }]
        });
        let keys = [[0xE0_u8; 16], [0xE1; 16], [0xE2; 16]];
        let messages = cases
            .iter()
            .zip(&recipients)
            .zip(&keys)
            .map(|(((subject, _), recipients), key)| UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    recipients,
                    ..edge_message(subject, key, true)
                },
                attachments: &[],
            })
            .collect::<Vec<_>>();
        drop(UnicodePstFile::create(&path, &edge_message("m31-first", &[0xDF; 16], true)).unwrap());
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &messages)
                .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut seen = 0;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let subject = match message.properties().get(0x0037) {
                Some(PropertyValue::Unicode(subject)) => subject.to_string(),
                other => panic!("{other:?}"),
            };
            if subject == "m31-first" {
                continue;
            }
            let table = message.recipient_table().unwrap();
            assert_row_without_gaps(table.context(), &subject);
            let values = table
                .rows_matrix()
                .next()
                .unwrap()
                .columns(table.context())
                .unwrap();
            let (column, value) = table
                .context()
                .columns()
                .iter()
                .zip(&values)
                .find(|(column, _)| column.prop_id() == 0x39FE)
                .unwrap();
            assert!(
                matches!(table.read_column(value.as_ref().unwrap(), column.prop_type()).unwrap(), PropertyValue::Unicode(email) if email.to_string() == "given@example.invalid"),
                "{subject}"
            );
            seen += 1;
        }
        assert_eq!(seen, cases.len());
    }

    #[test]
    fn given_properties_are_written_in_the_message_its_contents_row_and_its_recipients() {
        // Given tagged properties are written as given, in the message's property context (a long string in a subnode
        // included), in its contents-table row where the folder's contents table has the column with that type (0x0036,
        // 0x0057 and 0x0071 are columns of the template's Inbox), and as columns of its recipient table: the union of
        // the recipients' tags, a recipient without a tag having no cell. An attached message follows the same rules.
        // The test build checks every commit.
        let path = temp_pst("m21-given");
        let long = "Reporting MTA ".repeat(300);
        let multi = ["first", "zweite", "\u{fc}\u{e7}\u{fc}nc\u{fc}"];
        let message_properties = [
            UnicodePstProperty {
                id: 0x0036,
                value: UnicodePstValue::Integer32(2),
            },
            UnicodePstProperty {
                id: 0x0057,
                value: UnicodePstValue::Boolean(true),
            },
            UnicodePstProperty {
                id: 0x0071,
                value: UnicodePstValue::Binary(&[1, 2, 3, 4]),
            },
            UnicodePstProperty {
                id: 0x0015,
                value: UnicodePstValue::Time(133_750_000_000_000_000),
            },
            UnicodePstProperty {
                id: 0x1001,
                value: UnicodePstValue::Unicode("Report text, T\u{fc}rk\u{e7}e"),
            },
            UnicodePstProperty {
                id: 0x0049,
                value: UnicodePstValue::Unicode(""),
            },
            UnicodePstProperty {
                id: 0x0025,
                value: UnicodePstValue::Binary(&[]),
            },
            UnicodePstProperty {
                id: 0x7001,
                value: UnicodePstValue::MultipleUnicode(&multi),
            },
            UnicodePstProperty {
                id: 0x6820,
                value: UnicodePstValue::Unicode(&long),
            },
        ];
        let first = [
            UnicodePstProperty {
                id: 0x0C04,
                value: UnicodePstValue::Integer32(1),
            },
            UnicodePstProperty {
                id: 0x0C05,
                value: UnicodePstValue::Integer32(-1),
            },
            UnicodePstProperty {
                id: 0x0C20,
                value: UnicodePstValue::Integer32(511),
            },
            UnicodePstProperty {
                id: 0x0C21,
                value: UnicodePstValue::Unicode("mta.example.invalid"),
            },
            UnicodePstProperty {
                id: 0x0C1B,
                value: UnicodePstValue::Unicode("<mta.example.invalid #5.1.1>"),
            },
            UnicodePstProperty {
                id: 0x0032,
                value: UnicodePstValue::Time(133_750_000_010_000_000),
            },
            UnicodePstProperty {
                id: 0x0E25,
                value: UnicodePstValue::Boolean(true),
            },
        ];
        let second = [
            UnicodePstProperty {
                id: 0x0010,
                value: UnicodePstValue::Time(133_750_000_020_000_000),
            },
            UnicodePstProperty {
                id: 0x0C1B,
                value: UnicodePstValue::Unicode(""),
            },
        ];
        let recipients = [
            UnicodePstRecipient {
                name: "Failed",
                email: "failed@example.invalid",
                recipient_type: UnicodePstRecipientType::To,
                properties: &first,
            },
            UnicodePstRecipient {
                name: "Delivered",
                email: "delivered@example.invalid",
                recipient_type: UnicodePstRecipientType::Cc,
                properties: &second,
            },
            UnicodePstRecipient {
                name: "Plain",
                email: "plain@example.invalid",
                recipient_type: UnicodePstRecipientType::Bcc,
                properties: &[],
            },
        ];
        let inner = UnicodePstMessage {
            recipients: &recipients,
            properties: &message_properties,
            ..edge_message("m21-inner", &[0xF2; 16], true)
        };
        let attachments = [UnicodePstAttachment {
            filename: "Report.msg",
            mime_type: "",
            content_id: None,
            data: &[],
            stream: None,
            hidden: false,
            attached_message: Some(Box::new(UnicodePstAttachedMessage {
                message: inner,
                attachments: vec![],
            })),
        }];
        let outer = UnicodePstMessage {
            recipients: &recipients,
            properties: &message_properties,
            ..edge_message("m21-outer", &[0xF1; 16], true)
        };
        drop(UnicodePstFile::create(&path, &edge_message("m21-first", &[0xF0; 16], true)).unwrap());
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: outer,
                    attachments: &attachments,
                }],
            )
            .unwrap(),
        );

        let check = |message: &UnicodeMessage, label: &str| {
            for property in &message_properties {
                let got = message
                    .properties()
                    .get(property.id)
                    .unwrap_or_else(|| panic!("{label}: no {:#06x}", property.id));
                assert_eq!(
                    given_value(got),
                    wanted(&property.value),
                    "{label}: {:#06x}",
                    property.id
                );
            }
            let table = message.recipient_table().unwrap();
            let rows = table.rows_matrix().collect::<Vec<_>>();
            assert_eq!(rows.len(), 3, "{label}");
            for (row, recipient) in rows.iter().zip(&recipients) {
                let values = row.columns(table.context()).unwrap();
                for id in [
                    0x0C04_u16, 0x0C05, 0x0C20, 0x0C21, 0x0C1B, 0x0032, 0x0010, 0x0E25,
                ] {
                    let (column, value) = table
                        .context()
                        .columns()
                        .iter()
                        .zip(&values)
                        .find(|(column, _)| column.prop_id() == id)
                        .unwrap_or_else(|| panic!("{label}: no recipient column {id:#06x}"));
                    let given = recipient
                        .properties
                        .iter()
                        .find(|property| property.id == id);
                    match (value, given) {
                        (None, None) => {}
                        // HNID 0 with the existence bit set is a present, empty value; this crate's reader cannot read it.
                        (Some(TableRowColumnValue::Heap(heap)), Some(property))
                            if u32::from(*heap) == 0 =>
                        {
                            assert_eq!(
                                wanted(&property.value),
                                "empty",
                                "{label}: {} {id:#06x}",
                                recipient.name
                            );
                        }
                        (Some(value), Some(property)) => {
                            let read = table.read_column(value, column.prop_type()).unwrap();
                            assert_eq!(
                                given_value(&read),
                                wanted(&property.value),
                                "{label}: {} {id:#06x}",
                                recipient.name
                            );
                        }
                        (value, given) => panic!(
                            "{label}: {} {id:#06x}: cell {value:?}, given {given:?}",
                            recipient.name
                        ),
                    }
                }
                // The fixed columns are still there and right.
                let (column, value) = table
                    .context()
                    .columns()
                    .iter()
                    .zip(&values)
                    .find(|(column, _)| column.prop_id() == 0x39FE)
                    .unwrap();
                assert!(
                    matches!(table.read_column(value.as_ref().unwrap(), column.prop_type()).unwrap(), PropertyValue::Unicode(email) if email.to_string() == recipient.email)
                );
            }
            // MS-PST 2.3.4.1: the column descriptors are sorted by tag.
            let ids = table
                .context()
                .columns()
                .iter()
                .map(|column| column.prop_id())
                .collect::<Vec<_>>();
            assert!(
                ids.windows(2).all(|pair| pair[0] < pair[1]),
                "{label}: columns not sorted: {ids:04x?}"
            );
            assert_row_without_gaps(table.context(), label);
        };
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut outer_seen = false;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            if !matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(subject)) if subject.to_string() == "m21-outer")
            {
                continue;
            }
            outer_seen = true;
            check(&message, "outer");
            // The contents row has the given properties' values where it has their columns.
            let values = row.columns(contents.context()).unwrap();
            // Inbox's contents table has 0x0036, 0x0057 and 0x0071 (not 0x0015, which the message still has).
            for id in [0x0036_u16, 0x0057, 0x0071] {
                let (column, value) = contents
                    .context()
                    .columns()
                    .iter()
                    .zip(&values)
                    .find(|(column, _)| column.prop_id() == id)
                    .unwrap_or_else(|| {
                        panic!(
                            "no contents column {id:#06x}: {:04x?}",
                            contents
                                .context()
                                .columns()
                                .iter()
                                .map(|column| (column.prop_id(), column.prop_type()))
                                .collect::<Vec<_>>()
                        )
                    });
                let property = message_properties
                    .iter()
                    .find(|property| property.id == id)
                    .unwrap();
                let read = contents
                    .read_column(
                        value
                            .as_ref()
                            .unwrap_or_else(|| panic!("contents cell {id:#06x} absent")),
                        column.prop_type(),
                    )
                    .unwrap();
                assert_eq!(
                    given_value(&read),
                    wanted(&property.value),
                    "contents cell {id:#06x}"
                );
            }
            let table = message.attachment_table().unwrap();
            let attachment_row = table.rows_matrix().next().unwrap();
            let attachment = UnicodeAttachment::read(
                message.clone(),
                NodeId::from(u32::from(attachment_row.id())),
                None,
            )
            .unwrap();
            check(&attachment.attached_message().unwrap(), "inner");
        }
        assert!(outer_seen);
        std::fs::remove_file(path).unwrap();
    }

    /// The message with this subject in Inbox.
    fn inbox_message(store: &Rc<UnicodeStore>, subject: &str) -> Rc<UnicodeMessage> {
        let contents = folder_at_path(store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let found = contents
            .rows_matrix()
            .map(|row| {
                let entry_id = store.properties().make_entry_id(NodeId::from(u32::from(row.id()))).unwrap();
                UnicodeMessage::read(store.clone(), &entry_id, None).unwrap()
            })
            .find(|message| matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(value)) if value.to_string() == subject));
        found.unwrap_or_else(|| panic!("no message {subject:?}"))
    }

    /// The map's property context as (ID, bytes or integer) pairs.
    fn map_snapshot(store: &Rc<UnicodeStore>) -> Vec<(u16, Vec<u8>)> {
        let map = UnicodeNamedPropertyMap::read(store.clone()).unwrap();
        map.properties()
            .iter()
            .map(|(id, value)| {
                (
                    *id,
                    match value {
                        PropertyValue::Binary(value) => value.buffer().to_vec(),
                        PropertyValue::Integer32(value) => value.to_le_bytes().to_vec(),
                        other => panic!("{id:#06x}: {other:?}"),
                    },
                )
            })
            .collect()
    }

    const PSETID_APPOINTMENT: [u8; 16] = guid_bytes(0x0006_2002, 0, 0, OFFICE_GUID_TAIL);
    const PSETID_COMMON: [u8; 16] = guid_bytes(0x0006_2008, 0, 0, OFFICE_GUID_TAIL);
    const CUSTOM_SET: [u8; 16] = guid_bytes(
        0x1111_2222,
        0x3333,
        0x4444,
        [0x55, 0x55, 0x66, 0x66, 0x77, 0x77, 0x88, 0x88],
    );

    #[test]
    fn named_properties_already_in_the_map_resolve_to_its_entries_and_change_nothing_in_it() {
        // Anchored to the Outlook-made template's own map, not to the writer's reading of it: PSETID_Appointment LID
        // 0x8205 is its entry 0 (ID 0x8000), PS_PUBLIC_STRINGS "DRMLicense" its entry 10 (0x800A), PS_INTERNET_HEADERS
        // "content-class" its entry 9 (0x8009). The header name is given as "Content-Class" and matches after
        // lower-casing (MS-OXCPRPT). The map is unchanged.
        let path = temp_pst("m22-existing");
        drop(
            UnicodePstFile::create(&path, &edge_message("existing-first", &[0xB0; 16], true))
                .unwrap(),
        );
        let before = map_snapshot(
            &UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap(),
        );
        let named = [
            UnicodePstNamedProperty {
                set: PSETID_APPOINTMENT,
                name: UnicodePstPropertyName::Lid(0x8205),
                value: UnicodePstValue::Integer32(2),
            },
            UnicodePstNamedProperty {
                set: PS_PUBLIC_STRINGS_SET,
                name: UnicodePstPropertyName::Name("DRMLicense"),
                value: UnicodePstValue::Binary(&[1, 2, 3]),
            },
            UnicodePstNamedProperty {
                set: PS_INTERNET_HEADERS_SET,
                name: UnicodePstPropertyName::Name("Content-Class"),
                value: UnicodePstValue::Unicode("urn:content-classes:message"),
            },
        ];
        let message = UnicodePstMessage {
            named_properties: &named,
            ..edge_message("existing-named", &[0xB1; 16], true)
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        assert_eq!(map_snapshot(&store), before, "the map is unchanged");
        let message = inbox_message(&store, "existing-named");
        assert!(matches!(
            message.properties().get(0x8000),
            Some(PropertyValue::Integer32(2))
        ));
        assert!(
            matches!(message.properties().get(0x800A), Some(PropertyValue::Binary(value)) if value.buffer() == [1, 2, 3])
        );
        assert!(
            matches!(message.properties().get(0x8009), Some(PropertyValue::Unicode(value)) if value.to_string() == "urn:content-classes:message")
        );
        std::fs::remove_file(path).unwrap();
    }

    /// The map's streams, decoded here from MS-PST 2.4.7 (not with the crate's reader's hash lookup).
    fn decoded_map(
        store: &Rc<UnicodeStore>,
    ) -> (
        u32,
        Vec<[u8; 16]>,
        Vec<(u32, u16, u16)>,
        Vec<u8>,
        BTreeMap<u16, Vec<(u32, u16, u16)>>,
    ) {
        let snapshot = map_snapshot(store).into_iter().collect::<BTreeMap<_, _>>();
        let records = |data: &[u8]| {
            data.chunks(8)
                .map(|r| {
                    (
                        u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
                        u16::from_le_bytes([r[4], r[5]]),
                        u16::from_le_bytes([r[6], r[7]]),
                    )
                })
                .collect::<Vec<_>>()
        };
        let count = u32::from_le_bytes(snapshot[&0x0001].clone().try_into().unwrap());
        let guids = snapshot[&0x0002]
            .chunks(16)
            .map(|g| <[u8; 16]>::try_from(g).unwrap())
            .collect();
        let buckets = snapshot
            .iter()
            .filter(|(id, _)| **id >= 0x1000)
            .map(|(id, data)| (*id, records(data)))
            .collect();
        (
            count,
            guids,
            records(&snapshot[&0x0003]),
            snapshot[&0x0004].clone(),
            buckets,
        )
    }

    #[test]
    fn new_named_properties_are_added_to_the_map_as_the_template_lays_it_out() {
        // A new LID in a set the map has (PSETID_Common), a new set, new string names in PS_PUBLIC_STRINGS and
        // PS_INTERNET_HEADERS and in the new set; the same names again in the batch and in a later commit, with the
        // header in other cases. Each name gets one entry, in order of first use from 0x8023 (the template has 35): its
        // NAMEID in the entry stream, a string at a 4-byte boundary with its length and zero padding, the new GUID at
        // the stream's end, and the bucket record (the CRC of a string name, wGuid kept) in the bucket the formula
        // gives. A new session finds them again.
        let path = temp_pst("m22-new");
        drop(UnicodePstFile::create(&path, &edge_message("new-first", &[0xA0; 16], true)).unwrap());
        let first = [
            UnicodePstNamedProperty {
                set: PSETID_COMMON,
                name: UnicodePstPropertyName::Lid(0x8501),
                value: UnicodePstValue::Integer32(15),
            },
            UnicodePstNamedProperty {
                set: PS_PUBLIC_STRINGS_SET,
                name: UnicodePstPropertyName::Name("C9a test"),
                value: UnicodePstValue::Unicode("public"),
            },
            UnicodePstNamedProperty {
                set: PS_INTERNET_HEADERS_SET,
                name: UnicodePstPropertyName::Name("X-C9a-Header"),
                value: UnicodePstValue::Unicode("one"),
            },
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Lid(1),
                value: UnicodePstValue::Boolean(true),
            },
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Name("Custom name \u{fc}"),
                value: UnicodePstValue::Time(133_750_000_000_000_000),
            },
        ];
        let second = [
            UnicodePstNamedProperty {
                set: PS_INTERNET_HEADERS_SET,
                name: UnicodePstPropertyName::Name("x-c9a-header"),
                value: UnicodePstValue::Unicode("two"),
            },
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Name("Second"),
                value: UnicodePstValue::Binary(&[]),
            },
        ];
        let batch = [
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    named_properties: &first,
                    ..edge_message("new-a", &[0xA1; 16], true)
                },
                attachments: &[],
            },
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    named_properties: &second,
                    ..edge_message("new-b", &[0xA2; 16], true)
                },
                attachments: &[],
            },
        ];
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );
        let third = [
            UnicodePstNamedProperty {
                set: PS_INTERNET_HEADERS_SET,
                name: UnicodePstPropertyName::Name("X-C9A-HEADER"),
                value: UnicodePstValue::Unicode("three"),
            },
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Lid(2),
                value: UnicodePstValue::Integer32(-7),
            },
        ];
        let mut session = UnicodePstWriter::open(&path).unwrap();
        session
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        named_properties: &third,
                        ..edge_message("new-c", &[0xA3; 16], true)
                    },
                    attachments: &[],
                }],
            )
            .unwrap();
        session.close().unwrap();

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let (count, guids, entries, strings, buckets) = decoded_map(&store);
        assert_eq!(count, 251);
        assert_eq!(guids.len(), 7, "one new GUID");
        assert_eq!(guids[6], CUSTOM_SET);
        assert_eq!(entries.len(), 35 + 7);
        let utf16 = |text: &str| {
            text.encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        // (set's wGuid, LID or name) in order of first use; PSETID_Common is the template's GUID 5 (index 2 + 3).
        let wanted: [(u16, Result<u32, &str>); 7] = [
            (5, Ok(0x8501)),
            (2, Err("C9a test")),
            (4, Err("x-c9a-header")),
            (9, Ok(1)),
            (9, Err("Custom name \u{fc}")),
            (9, Err("Second")),
            (9, Ok(2)),
        ];
        for (position, (wguid, name)) in wanted.iter().enumerate() {
            let (dw, gw, prop_index) = entries[35 + position];
            assert_eq!(
                prop_index,
                35 + position as u16,
                "entry {position}: wPropIdx"
            );
            assert_eq!(gw >> 1, *wguid, "entry {position}: wGuid");
            let hash = match name {
                Ok(lid) => {
                    assert_eq!((dw, gw & 1), (*lid, 0), "entry {position}");
                    *lid
                }
                Err(text) => {
                    assert_eq!(gw & 1, 1, "entry {position}: N");
                    assert_eq!(dw % 4, 0, "entry {position}: offset on a 4-byte boundary");
                    let start = dw as usize;
                    let length =
                        u32::from_le_bytes(strings[start..start + 4].try_into().unwrap()) as usize;
                    assert_eq!(
                        &strings[start + 4..start + 4 + length],
                        utf16(text).as_slice(),
                        "entry {position}: name"
                    );
                    assert!(
                        strings[start + 4 + length..(start + 4 + length).next_multiple_of(4)]
                            .iter()
                            .all(|byte| *byte == 0),
                        "entry {position}: padding"
                    );
                    crate::crc::compute_crc(0, &utf16(text))
                }
            };
            let bucket = 0x1000 + ((hash ^ u32::from(gw)) % 251) as u16;
            assert!(
                buckets[&bucket].contains(&(hash, gw, prop_index)),
                "entry {position}: bucket {bucket:#06x}"
            );
        }
        assert_eq!(strings.len() % 4, 0);
        assert_eq!(
            buckets.values().map(Vec::len).sum::<usize>(),
            entries.len(),
            "one bucket record per entry"
        );
        assert!(
            buckets.values().all(|records| !records.is_empty()),
            "no empty bucket is stored"
        );
        let a = inbox_message(&store, "new-a");
        assert!(matches!(
            a.properties().get(0x8023),
            Some(PropertyValue::Integer32(15))
        ));
        assert!(
            matches!(a.properties().get(0x8025), Some(PropertyValue::Unicode(value)) if value.to_string() == "one")
        );
        assert!(matches!(
            a.properties().get(0x8027),
            Some(PropertyValue::Time(133_750_000_000_000_000))
        ));
        assert!(
            matches!(inbox_message(&store, "new-b").properties().get(0x8025), Some(PropertyValue::Unicode(value)) if value.to_string() == "two")
        );
        let c = inbox_message(&store, "new-c");
        assert!(
            matches!(c.properties().get(0x8025), Some(PropertyValue::Unicode(value)) if value.to_string() == "three")
        );
        assert!(matches!(
            c.properties().get(0x8029),
            Some(PropertyValue::Integer32(-7))
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn named_properties_that_break_the_rules_are_refused_before_anything_is_written() {
        // PS_MAPI (a tagged property), an empty name, a PS_INTERNET_HEADERS name that is not a header field name, the
        // categories' PidNameKeywords, a name twice (headers compare without case) and a negative time.
        let path = temp_pst("m22-refused");
        drop(
            UnicodePstFile::create(&path, &edge_message("refused-first", &[0x90; 16], true))
                .unwrap(),
        );
        let bytes = std::fs::read(&path).unwrap();
        let named = |set, name, value| UnicodePstNamedProperty { set, name, value };
        let int = UnicodePstValue::Integer32(1);
        let cases: Vec<(&str, Vec<UnicodePstNamedProperty>)> = vec![
            (
                "PS_MAPI",
                vec![named(PS_MAPI_SET, UnicodePstPropertyName::Lid(0x0037), int)],
            ),
            (
                "empty",
                vec![named(CUSTOM_SET, UnicodePstPropertyName::Name(""), int)],
            ),
            (
                "colon",
                vec![named(
                    PS_INTERNET_HEADERS_SET,
                    UnicodePstPropertyName::Name("X-A:B"),
                    int,
                )],
            ),
            (
                "space",
                vec![named(
                    PS_INTERNET_HEADERS_SET,
                    UnicodePstPropertyName::Name("X A"),
                    int,
                )],
            ),
            (
                "non-ASCII",
                vec![named(
                    PS_INTERNET_HEADERS_SET,
                    UnicodePstPropertyName::Name("X-\u{fc}"),
                    int,
                )],
            ),
            (
                "Keywords",
                vec![named(
                    PS_PUBLIC_STRINGS_SET,
                    UnicodePstPropertyName::Name("Keywords"),
                    int,
                )],
            ),
            (
                "twice",
                vec![
                    named(CUSTOM_SET, UnicodePstPropertyName::Lid(5), int),
                    named(CUSTOM_SET, UnicodePstPropertyName::Lid(5), int),
                ],
            ),
            (
                "header twice",
                vec![
                    named(
                        PS_INTERNET_HEADERS_SET,
                        UnicodePstPropertyName::Name("X-B"),
                        int,
                    ),
                    named(
                        PS_INTERNET_HEADERS_SET,
                        UnicodePstPropertyName::Name("x-b"),
                        int,
                    ),
                ],
            ),
            (
                "negative time",
                vec![named(
                    CUSTOM_SET,
                    UnicodePstPropertyName::Lid(6),
                    UnicodePstValue::Time(-1),
                )],
            ),
        ];
        for (label, properties) in &cases {
            let message = UnicodePstMessage {
                named_properties: properties,
                ..edge_message("refused", &[0x91; 16], true)
            };
            let error = UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .err()
            .unwrap_or_else(|| panic!("{label}: accepted"));
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "{label}: {error}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{label}: the file changed"
            );
        }
        // The same names, one of each per set and given once, are accepted: the rules refuse only what they name.
        let fine = [
            named(CUSTOM_SET, UnicodePstPropertyName::Lid(5), int),
            named(
                PS_INTERNET_HEADERS_SET,
                UnicodePstPropertyName::Name("X-B"),
                int,
            ),
        ];
        let message = UnicodePstMessage {
            named_properties: &fine,
            ..edge_message("fine", &[0x92; 16], true)
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap(),
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn thousands_of_names_move_the_map_streams_into_subnodes_and_pass_0x8fff() {
        // 4,200 new string names (42 messages of 100, over three commits) give IDs 0x8023 to 0x909A, past the 0x8FFF
        // that XstReader treats as the end of named IDs; the streams pass a heap allocation and are subnodes.
        let path = temp_pst("m22-many");
        drop(
            UnicodePstFile::create(&path, &edge_message("many-first", &[0x80; 16], true)).unwrap(),
        );
        let names = (0..4200)
            .map(|index| format!("Name {index:04}"))
            .collect::<Vec<_>>();
        let values = (0..4200).map(|index| index as i32).collect::<Vec<_>>();
        let named = names
            .iter()
            .zip(&values)
            .map(|(name, value)| UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Name(name),
                value: UnicodePstValue::Integer32(*value),
            })
            .collect::<Vec<_>>();
        let subjects = (0..42)
            .map(|index| format!("many-{index:02}"))
            .collect::<Vec<_>>();
        let keys = (0..42u8).map(|index| [index; 16]).collect::<Vec<_>>();
        let mut session = UnicodePstWriter::open(&path).unwrap();
        for commit in [0..14, 14..28, 28..42] {
            let batch = commit
                .map(|index| UnicodePstBatchMessage {
                    message: UnicodePstMessage {
                        named_properties: &named[index * 100..index * 100 + 100],
                        ..edge_message(&subjects[index], &keys[index], true)
                    },
                    attachments: &[],
                })
                .collect::<Vec<_>>();
            session
                .append_many_in_folder_with_attachments(&["Inbox"], &batch)
                .unwrap();
        }
        session.close().unwrap();
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let (_, _, entries, strings, _) = decoded_map(&store);
        assert_eq!(entries.len(), 35 + 4200);
        assert!(
            entries.len() * 8 > MAX_HN_ALLOCATION_SIZE && strings.len() > MAX_HN_ALLOCATION_SIZE
        );
        let last = inbox_message(&store, "many-41");
        assert!(
            matches!(
                last.properties().get(0x8023 + 4199),
                Some(PropertyValue::Integer32(4199))
            ),
            "ID 0x909a"
        );
        assert!(matches!(
            inbox_message(&store, "many-00").properties().get(0x8023),
            Some(PropertyValue::Integer32(0))
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_map_refuses_a_name_past_npid_0xfffe_and_reuses_a_set_s_guid() {
        // MS-PST 2.4.7.1, the NPID is 0x8000 + wPropIdx and the last one is 0xFFFE (0xFFFF is no property ID, as in the
        // readers' parsers). A map whose next index is 0x7FFE takes one more name and refuses the next by name. A set
        // already in the GUID stream is not appended again.
        let mut map = NameMap {
            others: BTreeMap::from([(0x0001, PropertyValue::Integer32(251))]),
            bucket_count: 251,
            guids: vec![CUSTOM_SET],
            entries: Vec::new(),
            strings: Vec::new(),
            buckets: BTreeMap::new(),
            index: HashMap::new(),
            next_index: 0x7FFE,
            changed: false,
        };
        assert_eq!(
            map.resolve(CUSTOM_SET, UnicodePstPropertyName::Lid(1))
                .unwrap(),
            0xFFFE
        );
        assert_eq!(map.guids.len(), 1, "the set's GUID is reused");
        assert_eq!(
            &map.entries[4..6],
            &((3_u16 << 1).to_le_bytes()),
            "wGuid 3: the GUID stream's first entry"
        );
        assert_eq!(
            map.resolve(CUSTOM_SET, UnicodePstPropertyName::Lid(1))
                .unwrap(),
            0xFFFE,
            "an existing name"
        );
        let error = map
            .resolve(CUSTOM_SET, UnicodePstPropertyName::Lid(2))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("0xFFFE"), "{error}");
    }

    #[test]
    fn a_named_property_fills_a_contents_column_with_its_id_and_type_only() {
        // Like a tagged one, a given named property fills a contents column that has the ID its name was given and its
        // type. No folder this writer makes has such a column (only the template's IPM subtree root's contents table
        // does), so the cell is checked directly.
        let named = [UnicodePstNamedProperty {
            set: PSETID_COMMON,
            name: UnicodePstPropertyName::Lid(0x8510),
            value: UnicodePstValue::Integer32(9),
        }];
        let input = UnicodePstMessage {
            named_properties: &named,
            ..edge_message("cell", &[0x70; 16], true)
        };
        let mut names = NameIds::default();
        names.given.insert(
            name_key(PSETID_COMMON, UnicodePstPropertyName::Lid(0x8510)),
            0x8010,
        );
        let message = NodeId::new(NodeIdType::NormalMessage, 0x400).unwrap();
        let cell = |prop_type, id| {
            new_message_table_cell(
                &TableColumnDescriptor::new(prop_type, id, 8, 4, 2),
                &input,
                &[],
                [0; 16],
                NID_ROOT_FOLDER,
                message,
                1,
                0,
                &names,
            )
            .unwrap()
        };
        assert!(matches!(
            cell(PropertyType::Integer32, 0x8010),
            Some(TableCell::Value(PropertyValue::Integer32(9)))
        ));
        assert!(
            cell(PropertyType::Unicode, 0x8010).is_none(),
            "another type"
        );
        assert!(
            cell(PropertyType::Integer32, 0x8011).is_none(),
            "another ID"
        );
    }

    #[test]
    fn given_values_too_large_for_a_heap_cell_are_written_in_subnodes_of_their_table() {
        // A table cell larger than a heap allocation (MS-PST 2.3.3: 3,580 bytes) is a subnode of NID_TYPE_LTP (MS-PST
        // 2.3.4.4: an HNID of a cell names a heap item or a subnode). A recipient's given string of 4,000 characters,
        // and a long recipient name, are such cells in the recipient table; a given binary of a contents column (Inbox
        // has 0x0071, PidTagConversationIndex) is one in the contents row.
        let path = temp_pst("m21-large-cells");
        let long = "Supplementary information. ".repeat(150);
        let name = "N".repeat(2000);
        let blob = vec![0x5A_u8; 5000];
        let first = [UnicodePstProperty {
            id: 0x0C1B,
            value: UnicodePstValue::Unicode(&long),
        }];
        let recipients = [
            UnicodePstRecipient {
                name: &name,
                email: "long@example.invalid",
                recipient_type: UnicodePstRecipientType::To,
                properties: &first,
            },
            UnicodePstRecipient {
                name: "Short",
                email: "short@example.invalid",
                recipient_type: UnicodePstRecipientType::Cc,
                properties: &[],
            },
        ];
        let properties = [UnicodePstProperty {
            id: 0x0071,
            value: UnicodePstValue::Binary(&blob),
        }];
        drop(
            UnicodePstFile::create(&path, &edge_message("large-first", &[0xC0; 16], true)).unwrap(),
        );
        let message = UnicodePstMessage {
            recipients: &recipients,
            properties: &properties,
            ..edge_message("large-cells", &[0xC1; 16], true)
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut seen = false;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            if !matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(subject)) if subject.to_string() == "large-cells")
            {
                continue;
            }
            seen = true;
            let (column, value) = contents
                .context()
                .columns()
                .iter()
                .zip(row.columns(contents.context()).unwrap())
                .find(|(column, _)| column.prop_id() == 0x0071)
                .unwrap();
            assert!(
                matches!(value, Some(TableRowColumnValue::Node(_))),
                "the 5,000-byte contents cell is a subnode: {value:?}"
            );
            assert!(
                matches!(contents.read_column(value.as_ref().unwrap(), column.prop_type()).unwrap(), PropertyValue::Binary(read) if read.buffer() == blob.as_slice())
            );
            let table = message.recipient_table().unwrap();
            let rows = table.rows_matrix().collect::<Vec<_>>();
            let values = rows[0].columns(table.context()).unwrap();
            for (id, wanted) in [(0x0C1B_u16, long.as_str()), (0x3001, name.as_str())] {
                let (column, value) = table
                    .context()
                    .columns()
                    .iter()
                    .zip(&values)
                    .find(|(column, _)| column.prop_id() == id)
                    .unwrap();
                assert!(
                    matches!(value, Some(TableRowColumnValue::Node(_))),
                    "{id:#06x} is a subnode: {value:?}"
                );
                assert!(
                    matches!(table.read_column(value.as_ref().unwrap(), column.prop_type()).unwrap(), PropertyValue::Unicode(read) if read.to_string() == wanted),
                    "{id:#06x}"
                );
            }
        }
        assert!(seen);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_given_property_of_another_type_than_its_contents_column_is_left_out_of_the_contents_row() {
        // A contents-table cell holds a given property only when the column has the property's type. Inbox's contents
        // table has 0x0036 (PidTagSensitivity) as PtypInteger32; given as a string, the message has it and the contents
        // row has no cell for it.
        let path = temp_pst("m21-contents-type");
        let properties = [UnicodePstProperty {
            id: 0x0036,
            value: UnicodePstValue::Unicode("Private"),
        }];
        drop(
            UnicodePstFile::create(&path, &edge_message("type-first", &[0xD0; 16], true)).unwrap(),
        );
        let message = UnicodePstMessage {
            properties: &properties,
            ..edge_message("type-other", &[0xD1; 16], true)
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let column = contents
            .context()
            .columns()
            .iter()
            .position(|column| column.prop_id() == 0x0036)
            .expect("Inbox has a 0x0036 column");
        assert_eq!(
            contents.context().columns()[column].prop_type(),
            PropertyType::Integer32
        );
        let mut seen = false;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            if matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(subject)) if subject.to_string() == "type-other")
            {
                assert!(
                    matches!(message.properties().get(0x0036), Some(PropertyValue::Unicode(value)) if value.to_string() == "Private")
                );
                assert!(
                    row.columns(contents.context()).unwrap()[column].is_none(),
                    "a string in an Integer32 contents cell"
                );
                seen = true;
            }
        }
        assert!(seen);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn an_empty_name_is_written_as_the_address_for_recipients_and_the_sender() {
        // MS-OXCMAIL 2.2.3.2, a recipient property group without a display name has the address as its display name:
        // PidTagDisplayName and PidTagRecipientDisplayName of a recipient, PidTagSenderName and
        // PidTagSentRepresentingName, the display-to list, and the contents row's sender name.
        let path = temp_pst("c10-empty-names");
        let recipients = [UnicodePstRecipient {
            name: "",
            email: "nameless@example.invalid",
            recipient_type: UnicodePstRecipientType::To,
            properties: &[],
        }];
        drop(
            UnicodePstFile::create(&path, &edge_message("names-first", &[0x60; 16], true)).unwrap(),
        );
        let message = UnicodePstMessage {
            recipients: &recipients,
            sender: Some(UnicodePstSender {
                name: "",
                email: "sender@example.invalid",
                address_type: "SMTP",
            }),
            ..edge_message("empty-names", &[0x61; 16], true)
        };
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message,
                    attachments: &[],
                }],
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let unicode = |value: Option<&PropertyValue>| match value {
            Some(PropertyValue::Unicode(value)) => value.to_string(),
            other => panic!("{other:?}"),
        };
        let mut seen = false;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            if unicode(message.properties().get(0x0037)) != "empty-names" {
                continue;
            }
            seen = true;
            for id in [0x0042_u16, 0x0C1A] {
                assert_eq!(
                    unicode(message.properties().get(id)),
                    "sender@example.invalid",
                    "{id:#06x}"
                );
            }
            assert_eq!(
                unicode(message.properties().get(0x0E04)),
                "nameless@example.invalid"
            );
            let values = row.columns(contents.context()).unwrap();
            let mut sender_cells = 0;
            for (column, value) in contents.context().columns().iter().zip(&values) {
                if matches!(column.prop_id(), 0x0042 | 0x0C1A) {
                    let read = contents
                        .read_column(value.as_ref().unwrap(), column.prop_type())
                        .unwrap();
                    assert!(
                        matches!(read, PropertyValue::Unicode(name) if name.to_string() == "sender@example.invalid"),
                        "contents {:#06x}",
                        column.prop_id()
                    );
                    sender_cells += 1;
                }
            }
            assert!(
                sender_cells > 0,
                "Inbox's contents table has a sender name column"
            );
            let table = message.recipient_table().unwrap();
            let row = table.rows_matrix().next().unwrap();
            let cells = row.columns(table.context()).unwrap();
            for (column, value) in table.context().columns().iter().zip(&cells) {
                if matches!(column.prop_id(), 0x3001 | 0x5FF6) {
                    let read = table
                        .read_column(value.as_ref().unwrap(), column.prop_type())
                        .unwrap();
                    assert!(
                        matches!(read, PropertyValue::Unicode(name) if name.to_string() == "nameless@example.invalid"),
                        "recipient {:#06x}",
                        column.prop_id()
                    );
                }
            }
        }
        assert!(seen);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_owned_lists_name_every_property_the_writer_writes_itself() {
        // A given property may not share an ID with one the writer writes, or the property context or the recipient
        // table would hold that ID twice. MESSAGE_OWNED_IDS and RECIPIENT_OWNED_IDS must therefore name every ID the
        // writer writes: here a message with every field the writer has set (sender, three kinds of recipient, HTML,
        // transport headers, categories, a submit time, a file and an attached message), and its attached message, a
        // draft.
        let path = temp_pst("m21-owned");
        let recipients = [
            UnicodePstRecipient {
                name: "To",
                email: "to@example.invalid",
                recipient_type: UnicodePstRecipientType::To,
                properties: &[],
            },
            UnicodePstRecipient {
                name: "",
                email: "cc@example.invalid",
                recipient_type: UnicodePstRecipientType::Cc,
                properties: &[],
            },
            UnicodePstRecipient {
                name: "Bcc",
                email: "bcc@example.invalid",
                recipient_type: UnicodePstRecipientType::Bcc,
                properties: &[],
            },
        ];
        let categories = ["Red", "Blue"];
        let full = |subject: &'static str, key: &'static [u8; 16]| UnicodePstMessage {
            recipients: &recipients,
            html_body: Some("<p>Body</p>"),
            client_submit_time: Some(133_750_070_000_000_000),
            categories: &categories,
            transport_headers: Some("Received: from a.example.invalid\r\n"),
            importance: UnicodePstImportance::High,
            ..edge_message(subject, key, false)
        };
        let inner = UnicodePstMessage {
            unsent: true,
            sender: None,
            message_id: None,
            delivery_time: None,
            client_submit_time: None,
            ..full("owned-inner", &[0xE2; 16])
        };
        let attachments = [
            UnicodePstAttachment {
                filename: "a.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &[1, 2, 3],
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "Inner.msg",
                mime_type: "",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: Some(Box::new(UnicodePstAttachedMessage {
                    message: inner,
                    attachments: vec![],
                })),
            },
        ];
        drop(
            UnicodePstFile::create(&path, &edge_message("owned-first", &[0xE0; 16], true)).unwrap(),
        );
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: full("owned-outer", &[0xE1; 16]),
                    attachments: &attachments,
                }],
            )
            .unwrap(),
        );
        let check = |message: &UnicodeMessage, label: &str| {
            // IDs from 0x8000 are named properties (the categories' PidNameKeywords is 0x8013 in the template), which no
            // given property may have.
            let ids = message
                .properties()
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| *id < 0x8000 && !MESSAGE_OWNED_IDS.contains(id))
                .collect::<Vec<_>>();
            assert!(
                ids.is_empty(),
                "{label}: the writer writes {ids:04x?}, not in MESSAGE_OWNED_IDS"
            );
            let table = message.recipient_table().unwrap();
            let columns = table
                .context()
                .columns()
                .iter()
                .map(|column| column.prop_id())
                .filter(|id| *id < 0x8000 && !RECIPIENT_OWNED_IDS.contains(id))
                .collect::<Vec<_>>();
            assert!(columns.is_empty(), "{label}: the writer writes recipient columns {columns:04x?}, not in RECIPIENT_OWNED_IDS");
        };
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut seen = 0;
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            check(&message, "message");
            seen += 1;
            if matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(subject)) if subject.to_string() == "owned-outer")
            {
                let table = message.attachment_table().unwrap();
                for attachment_row in table.rows_matrix() {
                    let attachment = UnicodeAttachment::read(
                        message.clone(),
                        NodeId::from(u32::from(attachment_row.id())),
                        None,
                    )
                    .unwrap();
                    if let Some(inner) = attachment.attached_message() {
                        check(&inner, "attached message");
                        seen += 1;
                    }
                }
            }
        }
        assert_eq!(seen, 3);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn given_properties_the_writer_owns_named_twice_negative_or_with_two_types_are_refused() {
        // A given property may not be one the writer writes itself, a named property (0x8000 and above), ID 0, a tag
        // twice in one message or recipient, a negative time, or one tag with two types among a message's recipients.
        // Each is invalid input, refused before anything is written.
        let path = temp_pst("m21-refused");
        drop(UnicodePstFile::create(&path, &edge_message("m21-first", &[0xF3; 16], true)).unwrap());
        let before = std::fs::read(&path).unwrap();
        let int = |id: u16| UnicodePstProperty {
            id,
            value: UnicodePstValue::Integer32(1),
        };
        let cases: Vec<(
            &str,
            Vec<UnicodePstProperty<'static>>,
            Vec<Vec<UnicodePstProperty<'static>>>,
        )> = vec![
            (
                "owned message class",
                vec![UnicodePstProperty {
                    id: 0x001A,
                    value: UnicodePstValue::Unicode("IPM.Note"),
                }],
                vec![],
            ),
            ("owned flags", vec![int(0x0E07)], vec![]),
            (
                "owned normalized subject",
                vec![UnicodePstProperty {
                    id: 0x0E1D,
                    value: UnicodePstValue::Unicode("x"),
                }],
                vec![],
            ),
            ("named", vec![int(0x8001)], vec![]),
            ("zero", vec![int(0)], vec![]),
            ("twice", vec![int(0x0036), int(0x0036)], vec![]),
            (
                "negative time",
                vec![UnicodePstProperty {
                    id: 0x0015,
                    value: UnicodePstValue::Time(-1),
                }],
                vec![],
            ),
            (
                "owned recipient column",
                vec![],
                vec![vec![UnicodePstProperty {
                    id: 0x3001,
                    value: UnicodePstValue::Unicode("x"),
                }]],
            ),
            (
                "recipient twice",
                vec![],
                vec![vec![int(0x0C04), int(0x0C04)]],
            ),
            (
                "two types",
                vec![],
                vec![
                    vec![int(0x0C21)],
                    vec![UnicodePstProperty {
                        id: 0x0C21,
                        value: UnicodePstValue::Unicode("x"),
                    }],
                ],
            ),
        ];
        for (label, message_properties, recipient_properties) in &cases {
            let recipients = recipient_properties
                .iter()
                .map(|properties| UnicodePstRecipient {
                    name: "r",
                    email: "r@example.invalid",
                    recipient_type: UnicodePstRecipientType::To,
                    properties,
                })
                .collect::<Vec<_>>();
            let input = UnicodePstMessage {
                recipients: &recipients,
                properties: message_properties,
                ..edge_message("m21-bad", &[0xF4; 16], true)
            };
            let error = UnicodePstFile::append_many_in_folder_with_attachments(
                &path,
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: input,
                    attachments: &[],
                }],
            )
            .map(drop)
            .unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidInput,
                "{label}: {error}"
            );
        }
        assert!(
            std::fs::read(&path).unwrap() == before,
            "a refused message wrote something"
        );
        std::fs::remove_file(path).unwrap();
    }

    fn attached_chain(depth: usize) -> Vec<UnicodePstAttachment<'static>> {
        let file = |name: &'static str| UnicodePstAttachment {
            filename: name,
            mime_type: "application/octet-stream",
            content_id: None,
            data: name.as_bytes(),
            stream: None,
            hidden: false,
            attached_message: None,
        };
        let mut attachments = vec![file("level-file.bin")];
        for level in (1..=depth).rev() {
            let subject: &'static str = Box::leak(format!("c2-level-{level}").into_boxed_str());
            let key: &'static [u8; 16] = Box::leak(Box::new([0xD0 + level as u8; 16]));
            let body: &'static str =
                Box::leak(format!("Body of level {level}, Türkçe 日本語").into_boxed_str());
            let message = UnicodePstMessage {
                recipients: TEST_RECIPIENTS,
                body,
                ..edge_message(subject, key, level % 2 == 0)
            };
            let name: &'static str = Box::leak(format!("Attached level {level}").into_boxed_str());
            attachments = vec![
                file("before.bin"),
                UnicodePstAttachment {
                    filename: name,
                    mime_type: "",
                    content_id: None,
                    data: &[],
                    stream: None,
                    hidden: false,
                    attached_message: Some(Box::new(UnicodePstAttachedMessage {
                        message,
                        attachments,
                    })),
                },
            ];
        }
        attachments
    }

    #[test]
    fn attached_emails_are_messages_inside_their_attachments_at_every_depth() {
        // An attached e-mail is PidTagAttachMethod 5 with PidTagAttachDataObject, whose PtypObject names a
        // NID_TYPE_NORMAL_MESSAGE subnode of the attachment node holding a complete message (MS-PST 2.4.6.2.2). Read
        // back through this crate's reader at depths 1, 2 and 8, level by level: the attachment's properties and table
        // row, the attached message's subject, body, recipients and files, and the sizes. The test build checks the
        // file after every commit.
        let path = temp_pst("c2-attached");
        let keys = [[0xC7_u8; 16], [0xC8; 16], [0xC9; 16]];
        let chains = [attached_chain(1), attached_chain(2), attached_chain(8)];
        let plain = edge_message("c2-depth-0", &[0xC6; 16], true);
        drop(UnicodePstFile::create(&path, &plain).unwrap());
        let batch = chains
            .iter()
            .zip(&keys)
            .enumerate()
            .map(|(index, (chain, key))| UnicodePstBatchMessage {
                message: edge_message(
                    ["c2-depth-1", "c2-depth-2", "c2-depth-8"][index],
                    key,
                    false,
                ),
                attachments: chain.as_slice(),
            })
            .collect::<Vec<_>>();
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );

        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let contents = folder_at_path(&store, &["Inbox"])
            .contents_table()
            .unwrap()
            .clone();
        let mut depths = Vec::new();
        for row in contents.rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let mut message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let Some(PropertyValue::Unicode(subject)) = message.properties().get(0x0037) else {
                panic!("subject")
            };
            let subject = subject.to_string();
            let mut depth = 0;
            loop {
                let Some(table) = message.attachment_table() else {
                    break;
                };
                let rows = table.rows_matrix().collect::<Vec<_>>();
                assert!(
                    matches!(
                        message.properties().get(0x0E1B),
                        Some(PropertyValue::Boolean(true))
                    ),
                    "{subject} at {depth}"
                );
                if rows.len() == 1 {
                    // The innermost level: one file.
                    let attachment = UnicodeAttachment::read(
                        message.clone(),
                        NodeId::from(u32::from(rows[0].id())),
                        None,
                    )
                    .unwrap();
                    match attachment.data() {
                        Some(AttachmentData::Binary(value)) => {
                            assert_eq!(value.buffer(), b"level-file.bin")
                        }
                        _ => {
                            panic!("{subject} at {depth}: the innermost attachment is not the file")
                        }
                    }
                    break;
                }
                assert_eq!(rows.len(), 2, "{subject} at {depth}");
                let file = UnicodeAttachment::read(
                    message.clone(),
                    NodeId::from(u32::from(rows[0].id())),
                    None,
                )
                .unwrap();
                assert!(
                    matches!(file.data(), Some(AttachmentData::Binary(value)) if value.buffer() == b"before.bin")
                );
                let attachment = UnicodeAttachment::read(
                    message.clone(),
                    NodeId::from(u32::from(rows[1].id())),
                    None,
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "{subject} at {depth}: attachment {:#x}: {error}",
                        u32::from(rows[1].id())
                    )
                });
                let properties = attachment.properties();
                depth += 1;
                assert!(
                    matches!(properties.get(0x3705), Some(PropertyValue::Integer32(5))),
                    "{subject} at {depth}: method"
                );
                assert!(
                    matches!(properties.get(0x3001), Some(PropertyValue::Unicode(name)) if name.to_string() == format!("Attached level {depth}"))
                );
                assert!(
                    properties.get(0x3704).is_none()
                        && properties.get(0x3707).is_none()
                        && properties.get(0x370E).is_none()
                );
                let Some(PropertyValue::Object(object)) = properties.get(0x3701) else {
                    panic!("{subject} at {depth}: no PtypObject")
                };
                assert_eq!(object.node().id_type().unwrap(), NodeIdType::NormalMessage);
                // The table row duplicates the attachment: method 5, the size, and no file name.
                let values = rows[1].columns(table.context()).unwrap();
                let column = |id: u16| {
                    table
                        .context()
                        .columns()
                        .iter()
                        .position(|column| column.prop_id() == id)
                        .unwrap()
                };
                let method = table
                    .read_column(
                        values[column(0x3705)].as_ref().unwrap(),
                        PropertyType::Integer32,
                    )
                    .unwrap();
                assert!(
                    matches!(method, PropertyValue::Integer32(5)),
                    "{subject} at {depth}: row method {method:?}"
                );
                assert!(
                    values[column(0x3704)].is_none(),
                    "{subject} at {depth}: the row has a file name"
                );
                let size = table
                    .read_column(
                        values[column(0x0E20)].as_ref().unwrap(),
                        PropertyType::Integer32,
                    )
                    .unwrap();
                assert!(
                    matches!((size, properties.get(0x0E20)), (PropertyValue::Integer32(row), Some(PropertyValue::Integer32(pc))) if row == *pc)
                );

                let attached = attachment.attached_message().expect("an attached message");
                assert!(matches!(
                    attachment.data(),
                    Some(AttachmentData::Message(_))
                ));
                let Some(PropertyValue::Unicode(inner)) = attached.properties().get(0x0037) else {
                    panic!("{subject} at {depth}: inner subject")
                };
                assert_eq!(inner.to_string(), format!("c2-level-{depth}"));
                let Some(PropertyValue::Unicode(body)) = attached.properties().get(0x1000) else {
                    panic!("inner body")
                };
                assert_eq!(
                    body.to_string(),
                    format!("Body of level {depth}, Türkçe 日本語")
                );
                assert!(
                    matches!(attached.properties().get(0x001A), Some(PropertyValue::Unicode(class)) if class.to_string() == "IPM.Note")
                );
                assert_eq!(
                    attached.recipient_table().unwrap().rows_matrix().count(),
                    TEST_RECIPIENTS.len()
                );
                // PtypObject's ulSize is the attached message's PidTagMessageSize, and PidTagAttachSize is the
                // attachment's values plus it (Outlook's rule, measured in an Outlook-written sample).
                let Some(PropertyValue::Integer32(attached_size)) =
                    attached.properties().get(0x0E08).cloned()
                else {
                    panic!("inner size")
                };
                assert_eq!(object.size() as i32, attached_size, "{subject} at {depth}");
                let Some(PropertyValue::Integer32(attach_size)) = properties.get(0x0E20).cloned()
                else {
                    panic!("attach size")
                };
                assert_eq!(
                    attach_size as usize,
                    object.size() as usize
                        + 13
                        + 2 * format!("Attached level {depth}").encode_utf16().count()
                );
                message = attached;
            }
            depths.push((subject, depth));
        }
        depths.sort();
        assert_eq!(
            depths,
            [
                ("c2-depth-0", 0),
                ("c2-depth-1", 1),
                ("c2-depth-2", 2),
                ("c2-depth-8", 8)
            ]
            .map(|(s, d)| (s.to_string(), d))
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn attached_emails_nest_at_most_32_levels() {
        // The writer's own bound on its recursion; 32 levels build, 33 are refused by kind.
        let message = edge_message("c2-depth-limit", &[0xCB; 16], true);
        assert!(message_parts(&message, &attached_chain(32), 1, &NameIds::default()).is_ok());
        let error = message_parts(&message, &attached_chain(33), 1, &NameIds::default())
            .err()
            .expect("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_attached_email_is_refused_with_content_or_without_a_name() {
        // An attached message carries no file content of its own, and its display name is required.
        let message = edge_message("c2-refused", &[0xCA; 16], true);
        for (data, content_id, filename) in [
            (&b"x"[..], None, "a"),
            (&b""[..], Some("<c@x>"), "a"),
            (&b""[..], None, ""),
        ] {
            let attached = UnicodePstAttachedMessage {
                message,
                attachments: Vec::new(),
            };
            let attachment = UnicodePstAttachment {
                filename,
                mime_type: "",
                content_id,
                data,
                stream: None,
                hidden: false,
                attached_message: Some(Box::new(attached)),
            };
            let error = message_parts(
                &message,
                std::slice::from_ref(&attachment),
                1,
                &NameIds::default(),
            )
            .err()
            .expect("refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn the_crate_s_hash_lookup_finds_every_map_entry_through_its_bucket() {
        // MS-PST 2.4.7.5, a string name's bucket record keeps wGuid, so the lookup hashes the CRC with (wGuid << 1) |
        // 1. Every entry of the template's map and of names the writer adds (a set of its own, wGuid 3 and up; internet
        // headers; a LID) is found through the crate's own hash_entry and hash_bucket.
        use crate::messaging::store::Store as _;
        let path = temp_pst("a13-lookup");
        drop(UnicodePstFile::create(&path, &edge_message("a13", &[0xA3; 16], true)).unwrap());
        let named = [
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Name("A13 custom"),
                value: UnicodePstValue::Integer32(1),
            },
            UnicodePstNamedProperty {
                set: CUSTOM_SET,
                name: UnicodePstPropertyName::Lid(7),
                value: UnicodePstValue::Integer32(2),
            },
            UnicodePstNamedProperty {
                set: PS_INTERNET_HEADERS_SET,
                name: UnicodePstPropertyName::Name("x-a13"),
                value: UnicodePstValue::Unicode("h"),
            },
            UnicodePstNamedProperty {
                set: PS_PUBLIC_STRINGS_SET,
                name: UnicodePstPropertyName::Name("A13 public"),
                value: UnicodePstValue::Unicode("p"),
            },
        ];
        let batch = [UnicodePstBatchMessage {
            message: UnicodePstMessage {
                named_properties: &named,
                ..edge_message("a13-named", &[0xA4; 16], true)
            },
            attachments: &[],
        }];
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let map = store.named_property_map().unwrap();
        let entries = map.properties().stream_entry().unwrap();
        assert!(
            entries.len() >= 35 + named.len(),
            "the template's 35 entries and the new ones"
        );
        let mut strings_with_a_guid = 0;
        for entry in entries {
            let record = map.properties().hash_entry(entry).unwrap();
            if matches!(
                entry.id(),
                crate::messaging::named_prop::NamedPropertyId::StringOffset(_)
            ) && u16::from(entry.guid()) != 0
            {
                strings_with_a_guid += 1;
            }
            assert!(
                map.properties()
                    .hash_bucket(&record)
                    .unwrap()
                    .contains(&record),
                "{entry:?} is not in its bucket"
            );
        }
        assert!(
            strings_with_a_guid >= 3,
            "string names with a wGuid other than 0 are among them"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_message_without_a_subnode_tree_is_read_with_no_recipient_or_attachment_table() {
        // Outlook writes messages whose NBT entry has no subnode tree (bidSub 0; 18 messages in two Outlook-written
        // samples, FAI items among them), against MS-PST 2.4.5.3's MUST. Here a written message's live NBT leaf entry
        // gets bidSub 0 (the page CRC recomputed, MS-PST 5.3); the crate reads its properties and no recipient or
        // attachment table.
        use crate::messaging::message::{Message as _, UnicodeMessage};
        use crate::messaging::store::Store as _;
        let path = temp_pst("a12-no-subnodes");
        let input = UnicodePstMessage {
            recipients: TEST_RECIPIENTS,
            ..edge_message("a12 no subnodes", &[0xA5; 16], true)
        };
        drop(UnicodePstFile::create(&path, &input).unwrap());
        let nid = {
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
            let inbox = folder_at_path(&store, &["Inbox"]);
            let row = inbox
                .contents_table()
                .unwrap()
                .rows_matrix()
                .next()
                .unwrap()
                .id();
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row)))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            assert!(
                message.recipient_table().is_some(),
                "the control: the written message has its recipient table"
            );
            u64::from(u32::from(row))
        };
        let mut bytes = std::fs::read(&path).unwrap();
        let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        // Unicode ROOT (MS-PST 2.2.2.5) at 180: dwReserved, ibFileEof, ibAMapLast, cbAMapFree, cbPMapFree, then BREFNBT,
        // whose ib follows its bid.
        let mut page = usize::try_from(u64_at(&bytes, 180 + 4 + 4 * 8 + 8)).unwrap();
        loop {
            let (count, size, level) = (
                usize::from(bytes[page + 488]),
                usize::from(bytes[page + 490]),
                bytes[page + 491],
            );
            assert_eq!(bytes[page + 496], 0x81, "an NBT page");
            let keys: Vec<u64> = (0..count)
                .map(|k| u64_at(&bytes, page + k * size))
                .collect();
            if level == 0 {
                let k = keys
                    .iter()
                    .position(|key| *key == nid)
                    .expect("the message's NBT entry");
                bytes[page + k * size + 16..page + k * size + 24].fill(0);
                let crc = crate::crc::compute_crc(0, &bytes[page..page + 496]);
                bytes[page + 500..page + 504].copy_from_slice(&crc.to_le_bytes());
                break;
            }
            let k = keys.iter().rposition(|key| *key <= nid).unwrap();
            page = usize::try_from(u64_at(&bytes, page + k * size + 16)).unwrap();
        }
        std::fs::write(&path, &bytes).unwrap();
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let entry_id = store
            .properties()
            .make_entry_id(NodeId::from(u32::try_from(nid).unwrap()))
            .unwrap();
        let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
        assert_eq!(message.properties().message_class().unwrap(), "IPM.Note");
        assert!(message.recipient_table().is_none());
        assert!(message.attachment_table().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_page_map_keeps_its_stored_free_count_and_refuses_more_freed_items_than_items() {
        // MS-PST 2.3.1.5 says only that cFree counts the freed items; another writer's zero-length allocation can be a
        // referenced empty value (enron.pst: 130, cFree 0). Three allocations, the second of zero length: cFree 0 and 1
        // are both read, and kept; cFree 4 (more than cAlloc 3) is refused.
        let page = |c_free: u16| {
            let mut bytes = Vec::new();
            for word in [3_u16, c_free, 12, 20, 20, 30] {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            bytes
        };
        for c_free in [0, 1] {
            let map = HeapNodePageMap::read(&mut Cursor::new(page(c_free))).unwrap();
            assert_eq!(map.allocations().len(), 3);
            let mut written = Vec::new();
            map.write(&mut written).unwrap();
            assert_eq!(written, page(c_free), "the stored cFree is kept");
        }
        assert!(HeapNodePageMap::read(&mut Cursor::new(page(4))).is_err());
    }

    fn temp_pst(tag: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "outlook-pst-m3-{tag}-{}-{stamp}.pst",
            std::process::id()
        ))
    }

    fn edge_message<'a>(
        subject: &'a str,
        key: &'a [u8; 16],
        is_read: bool,
    ) -> UnicodePstMessage<'a> {
        UnicodePstMessage {
            subject,
            recipients: &[],
            body: "Body",
            html_body: None,
            message_id: Some(subject),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: Some(key),
            is_read,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        }
    }

    /// Replace every occurrence of `find` after the header with `replace` (same length) and recompute the CRC of each
    /// Unicode data block that holds an edit (MS-PST 5.3), so the file stays block-valid. Returns the number of edits.
    /// In a permute-encoded file the patterns are encoded first; the encoding maps each byte on its own, so the encoded
    /// pattern is found exactly where the plain one is in a data block.
    fn patch_unencoded(path: &Path, find: &[u8], replace: &[u8]) -> usize {
        assert_eq!(find.len(), replace.len());
        let mut data = std::fs::read(path).unwrap();
        assert!(
            matches!(data[0x201], 0 | 1),
            "expects an unencoded or permute-encoded PST"
        );
        let (mut find, mut replace) = (find.to_vec(), replace.to_vec());
        if data[0x201] == 1 {
            crate::encode::permute::encode_block(&mut find);
            crate::encode::permute::encode_block(&mut replace);
        }
        let (find, replace) = (find.as_slice(), replace.as_slice());
        let mut edits = Vec::new();
        let mut start = 0x4400;
        while let Some(at) = data[start..].windows(find.len()).position(|w| w == find) {
            edits.push(start + at);
            start += at + 1;
        }
        let trailer_of = |data: &[u8], at: usize| -> (usize, usize, usize) {
            let mut q = at - at % 64 + 48;
            while q + 16 <= data.len() && q < at + 8192 + 64 {
                let cb = u16::from_le_bytes([data[q], data[q + 1]]) as usize;
                let crc = u32::from_le_bytes(data[q + 4..q + 8].try_into().unwrap());
                let span = (cb + 16).div_ceil(64) * 64;
                if q + 16 >= span {
                    let begin = q + 16 - span;
                    if begin <= at
                        && at < begin + cb
                        && crc == crc::compute_crc(0, &data[begin..begin + cb])
                    {
                        return (begin, cb, q);
                    }
                }
                q += 64;
            }
            panic!("no block trailer found for offset {at}");
        };
        let blocks: std::collections::BTreeSet<_> =
            edits.iter().map(|&at| trailer_of(&data, at)).collect();
        for &at in &edits {
            data[at..at + replace.len()].copy_from_slice(replace);
        }
        for (begin, cb, q) in blocks {
            let crc = crc::compute_crc(0, &data[begin..begin + cb]);
            data[q + 4..q + 8].copy_from_slice(&crc.to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
        edits.len()
    }

    fn hierarchy_counters(
        store: &Rc<UnicodeStore>,
        folder: &Rc<dyn Folder>,
    ) -> (PropertyValue, PropertyValue) {
        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        let parent = store
            .open_folder(&store.properties().make_entry_id(ipm).unwrap())
            .unwrap();
        let hierarchy = parent.hierarchy_table().unwrap();
        let row = hierarchy
            .find_row(TableRowId::new(u32::from(folder.properties().node_id())))
            .unwrap();
        let values = row.columns(hierarchy.context()).unwrap();
        let mut cells = (None, None);
        for (column, value) in hierarchy.context().columns().iter().zip(&values) {
            let value = || {
                hierarchy
                    .read_column(value.as_ref().unwrap(), column.prop_type())
                    .unwrap()
            };
            match column.prop_id() {
                0x3602 => cells.0 = Some(value()),
                0x3603 => cells.1 = Some(value()),
                _ => {}
            }
        }
        (cells.0.unwrap(), cells.1.unwrap())
    }

    /// A property context's BTH leaf holding a record whose wPropType the crate does not know makes the record reader
    /// fail in mid-leaf (`PropertyTreeRecordValue::read`). It must not return the records before it and drop the rest:
    /// an append reads the folder's property context and writes it back, and would silently lose every property after
    /// it. Here the Inbox's PidTagContentUnreadCount is stored with the type 0x00FB; the append is refused and the file
    /// left as it was.
    #[test]
    fn a_folder_property_of_an_unknown_type_refuses_the_append() {
        let keys = [[0xD5_u8; 16], [0xD6; 16]];
        let path = temp_pst("unknown-property-type");
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["Inbox"],
                &edge_message("<f4-1@example.com>", &keys[0], false),
            )
            .unwrap(),
        );
        let record = |ty: u16| {
            0x3603_u16
                .to_le_bytes()
                .iter()
                .chain(&ty.to_le_bytes())
                .chain(&1_i32.to_le_bytes())
                .copied()
                .collect::<Vec<_>>()
        };
        assert!(
            patch_unencoded(&path, &record(0x0003), &record(0x00FB)) > 0,
            "the stored unread count not found"
        );
        let before = std::fs::read(&path).unwrap();
        let result = UnicodePstFile::append_in_folder(
            &path,
            &["Inbox"],
            &edge_message("<f4-2@example.com>", &keys[1], true),
        );
        assert!(result.is_err(), "a folder property of an unknown type must refuse the append, not drop the properties after it");
        assert!(
            std::fs::read(&path).unwrap() == before,
            "the refused append changed the file"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// The unread count of an existing folder is checked against a recount of its contents rows: a negative, missing or
    /// wrongly typed stored PidTagContentUnreadCount is replaced by the recount (in the folder and its hierarchy row),
    /// and a stored value that disagrees with the recount (stale) is refused before anything is written.
    #[test]
    fn recounts_unread_messages_when_the_stored_count_is_not_trustworthy() {
        let keys: Vec<[u8; 16]> = (0..7u8).map(|i| [0xC0 + i; 16]).collect();
        let stored = 0x3603_u16
            .to_le_bytes()
            .iter()
            .chain(&3_u16.to_le_bytes())
            .chain(&2_i32.to_le_bytes())
            .copied()
            .collect::<Vec<_>>();
        let record = |id: u16, ty: u16, value: i32| {
            id.to_le_bytes()
                .iter()
                .chain(&ty.to_le_bytes())
                .chain(&value.to_le_bytes())
                .copied()
                .collect::<Vec<_>>()
        };
        for (name, replacement, refused) in [
            ("stale", record(0x3603, 3, 7), true),
            ("negative", record(0x3603, 3, -3), false),
            ("wrong-type", record(0x3603, 0x000A, 2), false),
            ("missing", record(0x3604, 3, 2), false),
        ] {
            let path = temp_pst(name);
            let first = edge_message("<m3-n2-1@example.com>", &keys[0], true);
            drop(UnicodePstFile::create_in_folder(&path, &["Inbox"], &first).unwrap());
            let more = [
                edge_message("<m3-n2-2@example.com>", &keys[1], false),
                edge_message("<m3-n2-3@example.com>", &keys[2], false),
                edge_message("<m3-n2-4@example.com>", &keys[3], true),
            ];
            drop(UnicodePstFile::append_many_in_folder(&path, &["Inbox"], &more).unwrap());
            assert!(
                patch_unencoded(&path, &stored, &replacement) > 0,
                "{name}: stored count not found"
            );
            let batch = [
                UnicodePstBatchMessage {
                    message: edge_message("<m3-n2-5@example.com>", &keys[4], false),
                    attachments: &[],
                },
                UnicodePstBatchMessage {
                    message: edge_message("<m3-n2-6@example.com>", &keys[5], false),
                    attachments: &[],
                },
                UnicodePstBatchMessage {
                    message: edge_message("<m3-n2-7@example.com>", &keys[6], true),
                    attachments: &[],
                },
            ];
            if refused {
                let before = std::fs::read(&path).unwrap();
                let error = UnicodePstFile::append_many_in_folder_with_attachments(
                    &path,
                    &["Inbox"],
                    &batch,
                )
                .err()
                .expect("a stored count that disagrees with the rows must be refused");
                assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{name}: {error}");
                assert!(
                    error
                        .to_string()
                        .contains("disagrees with 2 unread contents rows"),
                    "{name}: {error}"
                );
                assert!(
                    std::fs::read(&path).unwrap() == before,
                    "{name}: the refused append changed the file"
                );
                std::fs::remove_file(path).unwrap();
                continue;
            }
            drop(
                UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                    .unwrap(),
            );
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
            let inbox = folder_at_path(&store, &["Inbox"]);
            assert_eq!(inbox.properties().content_count().unwrap(), 7, "{name}");
            assert_eq!(
                inbox.properties().unread_count().unwrap(),
                4,
                "{name}: 2 existing + 2 appended unread"
            );
            let (content, unread) = hierarchy_counters(&store, &inbox);
            assert!(
                matches!(content, PropertyValue::Integer32(7)),
                "{name}: hierarchy 3602 {content:?}"
            );
            assert!(
                matches!(unread, PropertyValue::Integer32(4)),
                "{name}: hierarchy 3603 {unread:?}"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    /// A parent hierarchy table whose counter column is not Integer32 is refused before anything is written, instead of
    /// leaving the row's old counter cell next to new folder counters.
    #[test]
    fn refuses_a_hierarchy_counter_column_of_another_type() {
        let path = temp_pst("n3");
        let key = [0xD1_u8; 16];
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["Inbox"],
                &edge_message("<m3-n3-1@example.com>", &key, false),
            )
            .unwrap(),
        );
        assert!(
            patch_unencoded(
                &path,
                &0x3603_0003_u32.to_le_bytes(),
                &0x3603_000A_u32.to_le_bytes()
            ) > 0
        );
        let before = std::fs::read(&path).unwrap();
        let key2 = [0xD2_u8; 16];
        let error = UnicodePstFile::append_in_folder(
            &path,
            &["Inbox"],
            &edge_message("<m3-n3-2@example.com>", &key2, false),
        )
        .err()
        .expect("a non-Integer32 counter column must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("expected Integer32"), "{error}");
        assert!(
            std::fs::read(&path).unwrap() == before,
            "the refused append changed the file"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// Categories are written as PtypMultipleString under the PidNameKeywords ID of the store's own map, in input
    /// order; a message without categories has no such property.
    #[test]
    fn writes_categories_under_the_store_keywords_property_id() {
        let path = temp_pst("categories");
        let keys: Vec<[u8; 16]> = (0..3u8).map(|i| [0xF0 + i; 16]).collect();
        let first_categories = ["Müşteri", "Test", "日本"];
        let first = UnicodePstMessage {
            categories: &first_categories,
            ..edge_message("<m3-cat-1@example.com>", &keys[0], true)
        };
        drop(UnicodePstFile::create_in_folder(&path, &["Inbox"], &first).unwrap());
        let second_categories = ["Only"];
        let batch = [
            UnicodePstBatchMessage {
                message: edge_message("<m3-cat-2@example.com>", &keys[1], true),
                attachments: &[],
            },
            UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    categories: &second_categories,
                    ..edge_message("<m3-cat-3@example.com>", &keys[2], false)
                },
                attachments: &[],
            },
        ];
        drop(
            UnicodePstFile::append_many_in_folder_with_attachments(&path, &["Inbox"], &batch)
                .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let tag = keywords_property_id(&store).unwrap();
        assert!(tag >= 0x8000, "{tag:#x}");
        let inbox = folder_at_path(&store, &["Inbox"]);
        let mut seen = 0;
        for row in inbox.contents_table().unwrap().rows_matrix() {
            let entry_id = store
                .properties()
                .make_entry_id(NodeId::from(u32::from(row.id())))
                .unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            let properties = message.properties();
            let subject = match properties.get(0x0037).unwrap() {
                PropertyValue::Unicode(value) => value.to_string(),
                other => panic!("unexpected subject {other:?}"),
            };
            let expected: &[&str] = match subject.as_str() {
                "<m3-cat-1@example.com>" => &first_categories,
                "<m3-cat-2@example.com>" => &[],
                "<m3-cat-3@example.com>" => &second_categories,
                other => panic!("unexpected message {other}"),
            };
            match (properties.get(tag), expected.is_empty()) {
                (None, true) => {}
                (Some(PropertyValue::MultipleUnicode(values)), false) => {
                    let got: Vec<String> = values
                        .iter()
                        .map(|value| String::from_utf16(value.buffer()).unwrap())
                        .collect();
                    assert_eq!(got, expected, "{subject}");
                }
                (other, _) => panic!("{subject}: unexpected Keywords value {other:?}"),
            }
            seen += 1;
        }
        assert_eq!(seen, 3);
        std::fs::remove_file(path).unwrap();
    }

    /// The writer adds no map entries; a store whose map has no "Keywords" entry refuses categories before anything is
    /// written, and still accepts messages without categories.
    #[test]
    fn refuses_categories_when_the_map_has_no_keywords_entry() {
        let path = temp_pst("no-keywords");
        let keys: Vec<[u8; 16]> = (0..3u8).map(|i| [0xF8 + i; 16]).collect();
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["Inbox"],
                &edge_message("<m3-nk-1@example.com>", &keys[0], true),
            )
            .unwrap(),
        );
        let utf16 = |text: &str| {
            text.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        };
        assert!(patch_unencoded(&path, &utf16("Keywords"), &utf16("Keywordz")) > 0);
        let before = std::fs::read(&path).unwrap();
        let categories = ["Test"];
        let error = UnicodePstFile::append_in_folder(
            &path,
            &["Inbox"],
            &UnicodePstMessage {
                categories: &categories,
                ..edge_message("<m3-nk-2@example.com>", &keys[1], true)
            },
        )
        .err()
        .expect("categories without a Keywords map entry must be refused");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            std::fs::read(&path).unwrap() == before,
            "the refused append changed the file"
        );
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["Inbox"],
                &edge_message("<m3-nk-3@example.com>", &keys[2], true),
            )
            .unwrap(),
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_ipm_subtree_holds_its_root_and_every_folder_below_it_but_not_the_store_root() {
        // The check that keeps a folderless append out of folders Outlook does not show.
        let path = temp_pst("c6-ipm-subtree");
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["Inbox", "Sub"],
                &edge_message("<c6@example.com>", &[0xC6; 16], true),
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        let inbox = find_child_folder(&store, ipm, "Inbox").unwrap().unwrap();
        let sub = find_child_folder(&store, inbox, "Sub").unwrap().unwrap();
        for (node, inside) in [
            (ipm, true),
            (inbox, true),
            (sub, true),
            (NodeId::from(0x122_u32), false),
        ] {
            assert_eq!(
                in_ipm_subtree(&store, ipm, node).unwrap(),
                inside,
                "{:#x}",
                u32::from(node)
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    /// The default receive folder is the folder named exactly "Inbox" (as `create_unicode_pst` and `child_folder_names`
    /// match it), otherwise the store's receive-folder table (used only inside the IPM subtree); a folder that matches
    /// only case-insensitively is never chosen, and the lookup does not run when a folder is given.
    #[test]
    fn default_receive_folder_matches_inbox_exactly() {
        let path = temp_pst("n4");
        let keys: Vec<[u8; 16]> = (0..4u8).map(|i| [0xE0 + i; 16]).collect();
        drop(
            UnicodePstFile::create_in_folder(
                &path,
                &["inbox"],
                &edge_message("<m3-n4-1@example.com>", &keys[0], true),
            )
            .unwrap(),
        );
        // Control: the template's "Inbox" exists next to the user folder "inbox"; a folderless append lands in "Inbox".
        drop(
            UnicodePstFile::append(
                &path,
                &edge_message("<m3-n4-2@example.com>", &keys[1], false),
            )
            .unwrap(),
        );
        {
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
            assert_eq!(
                folder_at_path(&store, &["Inbox"])
                    .properties()
                    .content_count()
                    .unwrap(),
                1
            );
            assert_eq!(
                folder_at_path(&store, &["inbox"])
                    .properties()
                    .content_count()
                    .unwrap(),
                1
            );
        }
        // Rename "Inbox" (folder PC and hierarchy row): only the user folder "inbox" still matches
        // case-insensitively; the receive-folder table of the template names the root folder 0x122.
        assert!(
            patch_unencoded(
                &path,
                &"Inbox"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
                &"Xnbox"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            ) > 0
        );
        // That folder is outside the IPM subtree, so the folderless append is refused, and the case variant is never
        // chosen either.
        let error = UnicodePstFile::append(
            &path,
            &edge_message("<m3-n4-3@example.com>", &keys[2], false),
        )
        .err()
        .expect("the receive-folder table names a folder outside the IPM subtree");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().contains("0x122")
                && error.to_string().contains("outside the IPM subtree"),
            "{error}"
        );
        {
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
            assert_eq!(
                folder_at_path(&store, &["inbox"])
                    .properties()
                    .content_count()
                    .unwrap(),
                1,
                "never the case variant"
            );
            let root = store
                .open_folder(
                    &store
                        .properties()
                        .make_entry_id(NodeId::from(0x122_u32))
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                root.properties().content_count().unwrap(),
                0,
                "nothing filed in the root folder"
            );
        }
        // Disable the receive-folder table's folder column (0x6605 -> 0x6606): no default receive folder exists now.
        assert!(
            patch_unencoded(
                &path,
                &0x6605_0003_u32.to_le_bytes(),
                &0x6606_0003_u32.to_le_bytes()
            ) > 0
        );
        let error = UnicodePstFile::append(
            &path,
            &edge_message("<m3-n4-5@example.com>", &keys[2], false),
        )
        .err()
        .expect("no default receive folder exists");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        // A requested, existing folder does not depend on the default-folder lookup (which has no answer now). The
        // folder is the existing "inbox": appending to it creates no exact "Inbox" that the lookup could find.
        drop(
            UnicodePstFile::append_in_folder(
                &path,
                &["inbox"],
                &edge_message("<m3-n4-4@example.com>", &keys[3], false),
            )
            .unwrap(),
        );
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
        assert_eq!(
            folder_at_path(&store, &["inbox"])
                .properties()
                .content_count()
                .unwrap(),
            2
        );
        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        assert!(find_child_folder(&store, ipm, "Inbox").unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }
}

struct AllocationMapPageInfo<Pst>
where
    Pst: PstFile,
    <Pst as PstFile>::AllocationMapPage: AllocationMapPageReadWrite<Pst>,
{
    amap_page: <Pst as PstFile>::AllocationMapPage,
    free_space: u64,
}

type PstFileReadWriteBTree<Pst, BTree> = RootBTreePage<
    Pst,
    <BTree as RootBTree>::Entry,
    <BTree as RootBTree>::IntermediatePage,
    <BTree as RootBTree>::LeafPage,
>;

type PstFileReadWriteNodeBTree<Pst> = PstFileReadWriteBTree<Pst, <Pst as PstFile>::NodeBTree>;

type PstFileReadWriteBlockBTree<Pst> = PstFileReadWriteBTree<Pst, <Pst as PstFile>::BlockBTree>;

impl<Pst> PstFileInner<Pst>
where
    Pst: PstFile + PstFileLock<Pst>,
    <Pst as PstFile>::BlockId: BlockId<Index = <Pst as PstFile>::BTreeKey>
        + From<<<Pst as PstFile>::ByteIndex as ByteIndex>::Index>
        + Debug,
    <Pst as PstFile>::PageId: From<<<Pst as PstFile>::ByteIndex as ByteIndex>::Index> + Debug,
    <Pst as PstFile>::ByteIndex: ByteIndex<Index: TryFrom<u64>> + Debug,
    <Pst as PstFile>::BlockRef: Debug,
    <Pst as PstFile>::PageRef: Debug,
    <Pst as PstFile>::Root: RootReadWrite<Pst>,
    <Pst as PstFile>::Header: HeaderReadWrite<Pst>,
    <Pst as PstFile>::DensityListPage: DensityListPageReadWrite<Pst>,
    <Pst as PstFile>::PageTrailer: PageTrailerReadWrite,
    <Pst as PstFile>::BTreeKey: BTreePageKeyReadWrite,
    <Pst as PstFile>::NodeBTreeEntry: NodeBTreeEntryReadWrite,
    <Pst as PstFile>::NodeBTree: NodeBTreeReadWrite<Pst, <Pst as PstFile>::NodeBTreeEntry>,
    <<Pst as PstFile>::NodeBTree as RootBTree>::IntermediatePage:
        RootBTreeIntermediatePageReadWrite<
            Pst,
            <Pst as PstFile>::NodeBTreeEntry,
            <<Pst as PstFile>::NodeBTree as RootBTree>::LeafPage,
        >,
    <<<Pst as PstFile>::NodeBTree as RootBTree>::IntermediatePage as BTreePage>::Entry:
        BTreePageEntryReadWrite,
    <<Pst as PstFile>::NodeBTree as RootBTree>::LeafPage: RootBTreeLeafPageReadWrite<Pst>,
    <Pst as PstFile>::BlockBTreeEntry: BlockBTreeEntryReadWrite,
    <Pst as PstFile>::BlockBTree: BlockBTreeReadWrite<Pst, <Pst as PstFile>::BlockBTreeEntry>,
    <<Pst as PstFile>::BlockBTree as RootBTree>::IntermediatePage:
        RootBTreeIntermediatePageReadWrite<
            Pst,
            <Pst as PstFile>::BlockBTreeEntry,
            <<Pst as PstFile>::BlockBTree as RootBTree>::LeafPage,
        >,
    <<<Pst as PstFile>::BlockBTree as RootBTree>::IntermediatePage as BTreePage>::Entry:
        BTreePageEntryReadWrite,
    <<Pst as PstFile>::BlockBTree as RootBTree>::LeafPage: RootBTreeLeafPageReadWrite<Pst>,
    <Pst as PstFile>::BlockTrailer: BlockTrailerReadWrite,
    <Pst as PstFile>::AllocationMapPage: AllocationMapPageReadWrite<Pst>,
    <Pst as PstFile>::AllocationPageMapPage: AllocationPageMapPageReadWrite<Pst>,
    <Pst as PstFile>::FreeMapPage: FreeMapPageReadWrite<Pst>,
    <Pst as PstFile>::FreePageMapPage: FreePageMapPageReadWrite<Pst>,
    <Pst as PstFile>::DensityListPage: DensityListPageReadWrite<Pst>,
    <Pst as PstFile>::DataTreeBlock: IntermediateTreeBlockReadWrite,
    <Pst as PstFile>::DataTreeEntry:
        IntermediateTreeEntryReadWrite + From<<Pst as PstFile>::BlockId>,
    <Pst as PstFile>::DataBlock: BlockReadWrite + Clone,
    <Pst as PstFile>::SubNodeTreeBlockHeader: SubNodeTreeBlockHeaderReadWrite,
    <Pst as PstFile>::SubNodeTreeBlock: IntermediateTreeBlockReadWrite,
    <<Pst as PstFile>::SubNodeTreeBlock as IntermediateTreeBlock>::Entry:
        IntermediateTreeEntryReadWrite,
    <Pst as PstFile>::SubNodeBlock: IntermediateTreeBlockReadWrite,
    <<Pst as PstFile>::SubNodeBlock as IntermediateTreeBlock>::Entry:
        IntermediateTreeEntryReadWrite,
{
    fn read_from(mut reader: Box<dyn PstReader>) -> io::Result<Self> {
        let header = <<Pst as PstFile>::Header as HeaderReadWrite<Pst>>::read(&mut reader)?;
        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::read(&mut reader);
        Ok(Self {
            reader: Mutex::new(Box::new(reader)),
            writer: Err(PstError::OpenedReadOnly),
            header,
            density_list,
            node_cache: Default::default(),
            block_cache: Default::default(),
        })
    }

    fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let reader = Box::new(File::open(&path)?);
        let writer = OpenOptions::new()
            .write(true)
            .open(&path)
            .map(BufWriter::new)
            .map(Mutex::new)
            .map_err(|_| PstError::NoWriteAccess(path.as_ref().display().to_string()));
        Ok(Self {
            writer,
            ..Self::read_from(reader)?
        })
    }

    /// Begin a transaction by rebuilding the allocation map if needed and initializing the density
    /// list, then set [`AmapStatus::Invalid`] in the header till the transaction is finished.
    ///
    /// See also [Transactional Semantics](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/bc5a92df-7fc1-4dc2-9c7c-5677237dd73a).
    fn start_write(&mut self) -> io::Result<()> {
        self.rebuild_allocation_map()?;
        self.ensure_density_list()?;

        let header = {
            self.header.update_unique();

            let root = self.header.root_mut();
            root.set_amap_status(AmapStatus::Invalid);
            self.header.clone()
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()
    }

    /// Complete a transaction by writing the header and density list to the file, and setting
    /// [`AmapStatus::Valid2`].
    ///
    /// See also [Transactional Semantics](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/bc5a92df-7fc1-4dc2-9c7c-5677237dd73a).
    #[instrument(skip_all)]
    fn finish_write(&mut self) -> io::Result<()> {
        // Reset AmapStatus::Valid2 to complete the transaction and then rewrite the updated
        // density list.
        let header = {
            self.header.update_unique();
            let root = self.header.root_mut();
            root.set_amap_status(AmapStatus::Valid2);
            self.header.clone()
        };

        self.update_density_list_page_id()?;
        let density_list = {
            self.density_list.as_ref().ok().and_then(|dl| {
                <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                    dl.backfill_complete(),
                    dl.current_page(),
                    dl.entries(),
                    *dl.trailer(),
                )
                .ok()
            })
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()?;

        if let Some(density_list) = density_list {
            density_list.write(writer)?;
            writer.flush()?;
        }

        Ok(())
    }

    /// [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced)
    fn rebuild_allocation_map(&mut self) -> io::Result<()> {
        self.rebuild_allocation_map_from(None)
    }

    /// Rebuild the allocation map, marking `known` blocks `(offset, size)` and B-tree page
    /// offsets when the caller already has them instead of walking both B-trees on disk.
    fn rebuild_allocation_map_from(
        &mut self,
        known: Option<KnownAllocations<'_>>,
    ) -> io::Result<()> {
        let root = self.header.root();
        if AmapStatus::Invalid != root.amap_is_valid() {
            return Ok(());
        }

        let num_amap_pages = root.file_eof_index().index().into() - AMAP_FIRST_OFFSET;
        let num_amap_pages = num_amap_pages.div_ceil(AMAP_DATA_SIZE);

        let mut amap_pages: Vec<_> = (0..num_amap_pages)
            .map(|amap_index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        amap_index * AMAP_DATA_SIZE + AMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::AllocationMap,
                    0,
                    block_id,
                    0,
                );

                let mut map_bits = [0; mem::size_of::<MapBits>()];
                let reserved = map_page_count(amap_index);

                let free_space = AMAP_DATA_SIZE - reserved * PAGE_SIZE as u64;

                let reserved = &[0xFF; 4][..reserved as usize];
                map_bits[..reserved.len()].copy_from_slice(reserved);

                let amap_page =
                    <<Pst as PstFile>::AllocationMapPage as AllocationMapPageReadWrite<Pst>>::new(
                        map_bits, trailer,
                    )?;
                Ok(AllocationMapPageInfo::<Pst> {
                    amap_page,
                    free_space,
                })
            })
            .collect::<PstResult<Vec<_>>>()?;

        if let Some((blocks, pages)) = known {
            for page in pages {
                Self::mark_page_allocation(*page, &mut amap_pages)?;
            }
            for (offset, size) in blocks {
                Self::mark_block_allocation(*offset, *size, &mut amap_pages)?;
            }
        } else {
            let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
            let reader = &mut *reader;

            let node_btree =
                <Pst::NodeBTree as RootBTreeReadWrite>::read(reader, *root.node_btree())?;

            Self::mark_node_btree_allocations(
                reader,
                root.node_btree().index(),
                &node_btree,
                &mut amap_pages,
            )?;

            let block_btree =
                <Pst::BlockBTree as RootBTreeReadWrite>::read(reader, *root.block_btree())?;

            Self::mark_block_btree_allocations(
                reader,
                root.block_btree().index(),
                &block_btree,
                &mut amap_pages,
            )?;
        }

        let free_bytes =
            <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                amap_pages.iter().map(|page| page.free_space).sum(),
            )
            .map_err(|_| PstError::IntegerConversion)?;
        let free_bytes = <<Pst as PstFile>::ByteIndex as ByteIndexReadWrite>::new(free_bytes);

        let pmap_pages: Vec<_> = (0..num_amap_pages.div_ceil(PMAP_PAGE_COUNT))
            .map(|index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * PMAP_DATA_SIZE + PMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::AllocationPageMap,
                    0,
                    block_id,
                    0,
                );

                let map_bits = [0xFF; mem::size_of::<MapBits>()];

                let pmap_page =
                    <<Pst as PstFile>::AllocationPageMapPage as AllocationPageMapPageReadWrite<
                        Pst,
                    >>::new(map_bits, trailer)?;
                Ok(pmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        let fmap_pages: Vec<_> = (0..(num_amap_pages.max(FMAP_FIRST_SIZE) - FMAP_FIRST_SIZE)
            .div_ceil(FMAP_PAGE_COUNT))
            .map(|index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * FMAP_DATA_SIZE + FMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::FreeMap,
                    0,
                    block_id,
                    0,
                );

                // All 0xFF, as classic Outlook writes FMap pages and never updates them (session.rs,
                // `constant_map_pages`); exact values become too small for SCANPST once Outlook frees or adds space.
                let map_bits = [0xFF; mem::size_of::<MapBits>()];

                let fmap_page = <<Pst as PstFile>::FreeMapPage as FreeMapPageReadWrite<Pst>>::new(
                    map_bits, trailer,
                )?;
                Ok(fmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        let fpmap_pages: Vec<_> = (0..(num_amap_pages.max(FPMAP_FIRST_SIZE) - FPMAP_FIRST_SIZE)
            .div_ceil(FPMAP_PAGE_COUNT))
            .map(|index| {
                let index =
                    <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<u64>>::try_from(
                        index * FPMAP_DATA_SIZE + FPMAP_FIRST_OFFSET,
                    )
                    .map_err(|_| PstError::IntegerConversion)?;
                let block_id = <Pst as PstFile>::PageId::from(index);

                let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
                    PageType::FreePageMap,
                    0,
                    block_id,
                    0,
                );

                let map_bits = [0xFF; mem::size_of::<MapBits>()];

                let fpmap_page = <<Pst as PstFile>::FreePageMapPage as FreePageMapPageReadWrite<
                    Pst,
                >>::new(map_bits, trailer)?;
                Ok(fpmap_page)
            })
            .collect::<PstResult<Vec<_>>>()?;

        {
            // Encode every map page, then write only those that differ from the file: the maps
            // sit one per 248 KiB, so rewriting all of them scatters thousands of page writes
            // over a large file, which a slow disk then has to flush.
            let mut encoded: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut encode =
                |offset: u64, write: &dyn Fn(&mut Cursor<Vec<u8>>) -> io::Result<()>| {
                    let mut page = Cursor::new(Vec::with_capacity(PAGE_SIZE));
                    write(&mut page)?;
                    encoded.push((offset, page.into_inner()));
                    Ok::<_, io::Error>(())
                };
            for page in amap_pages.into_iter().map(|info| info.amap_page) {
                encode(page.trailer().block_id().into_u64(), &|f| {
                    <Pst::AllocationMapPage as AllocationMapPageReadWrite<Pst>>::write(&page, f)
                })?;
            }
            for page in pmap_pages.into_iter() {
                encode(page.trailer().block_id().into_u64(), &|f| {
                    <Pst::AllocationPageMapPage as AllocationPageMapPageReadWrite<Pst>>::write(
                        &page, f,
                    )
                })?;
            }
            for page in fmap_pages.into_iter() {
                encode(page.trailer().block_id().into_u64(), &|f| {
                    <Pst::FreeMapPage as FreeMapPageReadWrite<Pst>>::write(&page, f)
                })?;
            }
            for page in fpmap_pages.into_iter() {
                encode(page.trailer().block_id().into_u64(), &|f| {
                    <Pst::FreePageMapPage as FreePageMapPageReadWrite<Pst>>::write(&page, f)
                })?;
            }

            {
                let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
                let mut current = vec![0; PAGE_SIZE];
                encoded.retain(|(offset, page)| {
                    current.resize(page.len(), 0);
                    let unchanged = reader.seek(SeekFrom::Start(*offset)).is_ok()
                        && reader.read_exact(&mut current).is_ok()
                        && current == *page;
                    !unchanged
                });
            }

            let mut writer = self
                .writer
                .as_ref()?
                .lock()
                .map_err(|_| PstError::LockError)?;
            let writer = &mut *writer;
            for (offset, page) in encoded {
                writer.seek(SeekFrom::Start(offset))?;
                writer.write_all(&page)?;
            }

            writer.flush()?;
            // The maps must be durable before the header marks them valid.
            writer.get_ref().sync_data()?;
        }

        let header = {
            // MS-PST deprecates the header's rgbFM and rgbFP: they MUST be 0xFF, which is what Outlook writes and SCANPST restores.
            <<Pst as PstFile>::Header as HeaderReadWrite<Pst>>::first_free_map(&mut self.header)
                .fill(0xFF);
            <<Pst as PstFile>::Header as HeaderReadWrite<Pst>>::first_free_page_map(
                &mut self.header,
            )
            .fill(0xFF);
            self.header.update_unique();

            let root = self.header.root_mut();
            let amap_last_index =
                AMAP_FIRST_OFFSET + num_amap_pages.saturating_sub(1) * AMAP_DATA_SIZE;
            let amap_last_index = <<<Pst as PstFile>::ByteIndex as ByteIndex>::Index as TryFrom<
                u64,
            >>::try_from(amap_last_index)
            .map_err(|_| PstError::IntegerConversion)?;
            root.set_amap_last_index(<<Pst as PstFile>::ByteIndex as ByteIndexReadWrite>::new(
                amap_last_index,
            ));
            root.reset_free_size(free_bytes)?;
            root.set_amap_status(AmapStatus::Valid2);

            self.header.clone()
        };

        let mut writer = self
            .writer
            .as_ref()?
            .lock()
            .map_err(|_| PstError::LockError)?;
        let writer = &mut *writer;
        writer.seek(SeekFrom::Start(0))?;
        header.write(writer)?;
        writer.flush()
    }

    /// Recursively mark all of the pages in the [`Node BTree`](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/7d759bcb-7864-480c-8746-f6af913ab085).
    /// as allocated. This does not include any blocks referenced in the nodes or the sub-trees in
    /// those blocks, blocks will be marked by [`Self::mark_block_btree_allocations`].
    ///
    /// See also [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced).
    #[instrument(skip_all)]
    fn mark_node_btree_allocations<R: PstReader>(
        reader: &mut R,
        page_index: Pst::ByteIndex,
        node_btree: &PstFileReadWriteNodeBTree<Pst>,
        amap_pages: &mut Vec<AllocationMapPageInfo<Pst>>,
    ) -> io::Result<()> {
        Self::mark_page_allocation(page_index.index().into(), amap_pages)?;

        if let RootBTreePage::Intermediate(page, ..) = node_btree {
            let level = page.level();
            for entry in page.entries() {
                let block = entry.block();
                let node_btree = <Pst::NodeBTree as RootBTreeReadWrite>::read(reader, block)?;
                match &node_btree {
                    RootBTreePage::Intermediate(page, ..) if page.level() + 1 != level => {
                        error!(
                            name: "PstUnexpectedBTreeIntermediatePage",
                            block = ?block.block(),
                            index = ?block.index(),
                            parent = level,
                            child = page.level(),
                            "Possible NBT page cycle detected, expected child == parent - 1"
                        );
                        return Err(PstError::InvalidBTreePage(block.index().index().into()).into());
                    }
                    RootBTreePage::Leaf(_) if level != 1 => {
                        error!(
                            name: "PstUnexpectedBTreeLeafPage",
                            block = ?block.block(),
                            index = ?block.index(),
                            parent = level,
                            child = page.level(),
                            "Corrupted NBT intermediate page detected, unexpected child leaf page"
                        );
                        return Err(PstError::InvalidBTreePage(block.index().index().into()).into());
                    }
                    _ => (),
                }
                Self::mark_node_btree_allocations(reader, block.index(), &node_btree, amap_pages)?;
            }
        }

        Ok(())
    }

    /// Recursively mark all of the pages and blocks in the [`Block BTree`](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/7d759bcb-7864-480c-8746-f6af913ab085).
    ///
    /// See also [Crash Recovery and AMap Rebuilding](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/d9bcc1fd-c66a-41b3-b6d7-ed09d2a25ced).
    #[instrument(skip_all)]
    fn mark_block_btree_allocations<R: PstReader>(
        reader: &mut R,
        page_index: Pst::ByteIndex,
        block_btree: &PstFileReadWriteBlockBTree<Pst>,
        amap_pages: &mut Vec<AllocationMapPageInfo<Pst>>,
    ) -> io::Result<()> {
        Self::mark_page_allocation(page_index.index().into(), amap_pages)?;

        match block_btree {
            RootBTreePage::Intermediate(page, ..) => {
                let level = page.level();
                for entry in page.entries() {
                    let block = entry.block();
                    let block_btree = <Pst::BlockBTree as RootBTreeReadWrite>::read(reader, block)?;
                    match &block_btree {
                        RootBTreePage::Intermediate(page, ..) if page.level() + 1 != level => {
                            error!(
                                name: "PstUnexpectedBTreeIntermediatePage",
                                block = ?block.block(),
                                index = ?block.index(),
                                parent = level,
                                child = page.level(),
                                "Possible BBT page cycle detected, expected child == parent - 1"
                            );
                            return Err(
                                PstError::InvalidBTreePage(block.index().index().into()).into()
                            );
                        }
                        RootBTreePage::Leaf(_) if level != 1 => {
                            error!(
                                name: "PstUnexpectedBTreeLeafPage",
                                block = ?block.block(),
                                index = ?block.index(),
                                parent = level,
                                child = page.level(),
                                "Corrupted BBT intermediate page detected, unexpected child leaf page"
                            );
                            return Err(
                                PstError::InvalidBTreePage(block.index().index().into()).into()
                            );
                        }
                        _ => (),
                    }
                    Self::mark_block_btree_allocations(
                        reader,
                        block.index(),
                        &block_btree,
                        amap_pages,
                    )?;
                }
            }
            RootBTreePage::Leaf(page) => {
                for entry in page.entries() {
                    Self::mark_block_allocation(
                        entry.block().index().index().into(),
                        entry.size(),
                        amap_pages,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Mark a page at the given file offset as allocated.
    fn mark_page_allocation(
        index: u64,
        amap_pages: &mut [AllocationMapPageInfo<Pst>],
    ) -> io::Result<()> {
        let index = index - AMAP_FIRST_OFFSET;
        let amap_index =
            usize::try_from(index / AMAP_DATA_SIZE).map_err(|_| PstError::IntegerConversion)?;
        let entry = amap_pages
            .get_mut(amap_index)
            .ok_or(PstError::AllocationMapPageNotFound(amap_index))?;
        entry.free_space -= PAGE_SIZE as u64;

        let bytes = entry.amap_page.map_bits_mut();

        let bit_index = usize::try_from((index % AMAP_DATA_SIZE) / 64)
            .map_err(|_| PstError::IntegerConversion)?;
        let byte_index = bit_index / 8;
        let bit_index = bit_index % 8;

        if bit_index == 0 {
            bytes[byte_index] = 0xFF;
        } else {
            let mask = 0x80_u8 >> bit_index;
            let mask = mask | (mask - 1);
            bytes[byte_index] |= mask;
            bytes[byte_index + 1] |= !mask;
        }

        Ok(())
    }

    /// Mark a block at the given file offset and size as allocated.
    fn mark_block_allocation(
        index: u64,
        size: u16,
        amap_pages: &mut [AllocationMapPageInfo<Pst>],
    ) -> io::Result<()> {
        let index = index - AMAP_FIRST_OFFSET;
        let amap_index =
            usize::try_from(index / AMAP_DATA_SIZE).map_err(|_| PstError::IntegerConversion)?;
        let entry = amap_pages
            .get_mut(amap_index)
            .ok_or(PstError::AllocationMapPageNotFound(amap_index))?;
        let size = u64::from(block_size(
            size + <<Pst as PstFile>::BlockTrailer as BlockTrailerReadWrite>::SIZE,
        ));
        entry.free_space -= size;

        let bytes = entry.amap_page.map_bits_mut();

        let bit_start = usize::try_from((index % AMAP_DATA_SIZE) / 64)
            .map_err(|_| PstError::IntegerConversion)?;
        let bit_end =
            bit_start + usize::try_from(size / 64).map_err(|_| PstError::IntegerConversion)?;
        let byte_start = bit_start / 8;
        let bit_start = bit_start % 8;
        let byte_end = bit_end / 8;
        let bit_end = bit_end % 8;

        if byte_start == byte_end {
            // The allocation fits in a single byte
            if bit_end > bit_start {
                let mask_start = 0x80_u8 >> bit_start;
                let mask_start = mask_start | (mask_start - 1);
                let mask_end = 0x80_u8 >> bit_end;
                let mask_end = !(mask_end | (mask_end - 1));
                let mask = mask_start & mask_end;
                bytes[byte_start] |= mask;
            }
            return Ok(());
        }

        let byte_start = if bit_start == 0 {
            byte_start
        } else {
            let mask_start = 0x80_u8 >> bit_start;
            let mask_start = mask_start | (mask_start - 1);
            bytes[byte_start] |= mask_start;
            byte_start + 1
        };

        if bit_end != 0 {
            let mask_end = 0x80_u8 >> bit_end;
            let mask_end = !(mask_end | (mask_end - 1));
            bytes[byte_end] |= mask_end;
        };

        if byte_end > byte_start {
            for byte in bytes[byte_start..byte_end].iter_mut() {
                *byte = 0xFF;
            }
        }

        Ok(())
    }

    /// Initialize the density list at the beginning of a transaction if it is missing, corrupt, or
    /// the page ID doesn't match the next page ID in the header.
    fn ensure_density_list(&mut self) -> PstResult<()> {
        if let Ok(density_list) = self.density_list.as_ref() {
            if density_list.trailer().block_id() == self.header.next_page() {
                return Ok(());
            }
        }

        let current_page = u32::try_from(
            (self.header.root().amap_last_index().index().into() - AMAP_FIRST_OFFSET)
                / AMAP_DATA_SIZE,
        )
        .map_err(|_| PstError::IntegerConversion)?;
        let block_id = self.header.next_page();
        let signature = PageType::DensityList
            .signature(ndb::page::DENSITY_LIST_FILE_OFFSET, block_id.into_u64());
        let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
            PageType::DensityList,
            signature,
            block_id,
            0,
        );
        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                false,
                current_page,
                &[],
                trailer,
            )?;

        self.density_list = Ok(density_list);
        Ok(())
    }

    /// Similar to [`Self::ensure_density_list`], but instead of resetting the density list, it
    /// assumes that it's already initialized and only updates the page ID if it doesn't match.
    fn update_density_list_page_id(&mut self) -> PstResult<()> {
        let Ok(density_list) = self.density_list.as_ref() else {
            return Ok(());
        };

        let next_page = self.header.next_page();
        if density_list.trailer().block_id() == next_page {
            return Ok(());
        }

        let signature = PageType::DensityList
            .signature(ndb::page::DENSITY_LIST_FILE_OFFSET, next_page.into_u64());
        let trailer = <<Pst as PstFile>::PageTrailer as PageTrailerReadWrite>::new(
            PageType::DensityList,
            signature,
            next_page,
            0,
        );

        let density_list =
            <<Pst as PstFile>::DensityListPage as DensityListPageReadWrite<Pst>>::new(
                density_list.backfill_complete(),
                density_list.current_page(),
                density_list.entries(),
                trailer,
            )?;

        self.density_list = Ok(density_list);
        Ok(())
    }

    fn read_node(&self, node: NodeId) -> io::Result<<Pst as PstFile>::NodeBTreeEntry> {
        let node_btree = *self.header.root().node_btree();
        let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
        let reader = &mut *reader;
        let node_btree =
            <<Pst as PstFile>::NodeBTree as RootBTreeReadWrite>::read(reader, node_btree)?;
        let mut page_cache = self.node_cache.borrow_mut();
        let node_id: <Pst as PstFile>::BTreeKey = u32::from(node).into();
        let node = node_btree.find_entry(reader, node_id, &mut page_cache)?;
        Ok(node)
    }

    fn read_block(&self, block: <Pst as PstFile>::BlockId) -> io::Result<Vec<u8>> {
        let encoding = self.header.crypt_method();
        let block_btree = *self.header.root().block_btree();
        let mut reader = self.reader.lock().map_err(|_| PstError::LockError)?;
        let reader = &mut *reader;
        let block_btree =
            <<Pst as PstFile>::BlockBTree as RootBTreeReadWrite>::read(reader, block_btree)?;
        let mut page_cache = self.block_cache.borrow_mut();
        let block = block_btree.find_entry(reader, block.search_key(), &mut page_cache)?;
        let block = DataTree::<Pst>::read(reader, encoding, &block)?;
        let mut block_cache = Default::default();
        let mut data = vec![];
        let _ = block
            .reader(
                reader,
                encoding,
                &block_btree,
                &mut page_cache,
                &mut block_cache,
            )?
            .read_to_end(&mut data)?;
        Ok(data)
    }
}

pub fn open_store(path: impl AsRef<Path>) -> io::Result<Rc<dyn Store>> {
    Ok(if let Ok(pst_file) = UnicodePstFile::open(path.as_ref()) {
        UnicodeStore::read(Rc::new(pst_file))?
    } else {
        let pst_file = AnsiPstFile::open(path.as_ref())?;
        AnsiStore::read(Rc::new(pst_file))?
    })
}

// Cell tests, kept at the end of the file, after every item the release build compiles, so that adding them moves no
// line of release code (panic locations carry line numbers).
#[cfg(test)]
mod h_cell_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn absent_values_fill_no_contents_cell_whatever_the_columns() {
        // A message without a sender, Message-ID or delivery time gets no contents cell for them in a table that has
        // those columns, and gets each cell when the value is given. Of the folders a library user can file into, none
        // of the bundled template's has the Message-ID column (only its IPM subtree root's table does), so
        // create_tests' `a_message_is_written_with_its_class_flags_and_times_and_without_what_it_does_not_have` never
        // meets it; a PST Outlook made can have it (pstsdk's sample1.pst). The cells are checked directly here.
        let names = NameIds::default();
        let message = NodeId::new(NodeIdType::NormalMessage, 0x400).unwrap();
        let key = [0x71_u8; 16];
        let given = UnicodePstMessage {
            subject: "cells",
            recipients: &[],
            body: "Body",
            html_body: None,
            message_id: Some("<cells@example.invalid>"),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: Some(&key),
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "Sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let absent = UnicodePstMessage {
            sender: None,
            message_id: None,
            delivery_time: None,
            ..given
        };
        let cell = |input: &UnicodePstMessage<'_>, prop_type, id| {
            new_message_table_cell(
                &TableColumnDescriptor::new(prop_type, id, 8, 4, 2),
                input,
                &[],
                [0; 16],
                NID_ROOT_FOLDER,
                message,
                1,
                0,
                &names,
            )
            .unwrap()
        };
        for (prop_type, id) in [
            (PropertyType::Unicode, 0x1035_u16),
            (PropertyType::Unicode, 0x0042),
            (PropertyType::Unicode, 0x0C1A),
            (PropertyType::Unicode, 0x0C1F),
            (PropertyType::Unicode, 0x0065),
            (PropertyType::Unicode, 0x0C1E),
            (PropertyType::Unicode, 0x0064),
            (PropertyType::Time, 0x0E06),
        ] {
            assert!(
                cell(&given, prop_type, id).is_some(),
                "{id:#06x}: given, a cell"
            );
            assert!(
                cell(&absent, prop_type, id).is_none(),
                "{id:#06x}: absent, no cell"
            );
        }
    }

    #[test]
    fn only_an_inline_attachment_is_marked_as_rendered_in_html() {
        // ATT_MHTML_REF (PidTagAttachFlags 0x4) is MS-OXCMAIL's inline mark; a listed attachment with a Content-ID
        // keeps the Content-ID without it, or a client may hide it (2.2.3.4.1.4.1).
        let attachment = |hidden| UnicodePstAttachment {
            filename: "a.png",
            mime_type: "image/png",
            content_id: Some("a@example.invalid"),
            data: b"png",
            stream: None,
            hidden,
            attached_message: None,
        };
        for (hidden, flags) in [(true, Some(4)), (false, None)] {
            let properties = attachment_properties(&attachment(hidden)).unwrap();
            assert!(
                properties.iter().any(|(id, _)| *id == 0x3712),
                "hidden {hidden}: Content-ID kept"
            );
            let got =
                properties
                    .iter()
                    .find(|(id, _)| *id == 0x3714)
                    .map(|(_, value)| match value {
                        PropertyValue::Integer32(value) => *value,
                        other => panic!("{other:?}"),
                    });
            assert_eq!(got, flags, "hidden {hidden}");
        }
    }

    #[test]
    fn a_stored_internet_header_name_matches_whatever_its_case() {
        // A name already in a foreign map, in PS_INTERNET_HEADERS, is matched lower-cased like a given one.
        let units = |name: &str| name.encode_utf16().collect::<Vec<_>>();
        let given = name_key(
            PS_INTERNET_HEADERS_SET,
            UnicodePstPropertyName::Name("X-Foo"),
        )
        .1;
        assert_eq!(
            stored_name_kind(PS_INTERNET_HEADERS_SET, units("X-Foo")),
            given
        );
        assert_eq!(
            stored_name_kind(PS_INTERNET_HEADERS_SET, units("x-foo")),
            given
        );
        // Elsewhere a string name matches exactly.
        assert_eq!(
            stored_name_kind(PS_PUBLIC_STRINGS_SET, units("X-Foo")),
            NameKind::Name(units("X-Foo"))
        );
        assert_ne!(
            stored_name_kind(PS_PUBLIC_STRINGS_SET, units("X-Foo")),
            name_key(PS_PUBLIC_STRINGS_SET, UnicodePstPropertyName::Name("x-foo")).1
        );
    }

    #[test]
    fn a_rebuilt_contents_table_keeps_present_empty_cells() {
        // A table rebuild that encodes an old row again (a contents table whose rows the session cannot extend in
        // place, as with an Outlook PST's in-heap rows, or a hierarchy row's update) must keep a present, empty value
        // (HNID 0 with its existence bit) present; read back as absent, the cell would no longer match its object's
        // empty property. The writer's own appends extend the row matrix and keep it (checked first).
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outlook-pst-empty-cell-{}-{stamp}.pst",
            std::process::id()
        ));
        let (key_1, key_2) = ([0x51_u8; 16], [0x52_u8; 16]);
        let first = UnicodePstMessage {
            subject: "",
            recipients: &[],
            body: "Body",
            html_body: None,
            message_id: None,
            delivery_time: None,
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: Some(&key_1),
            is_read: true,
            importance: UnicodePstImportance::Normal,
            sender: None,
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        };
        let contents = || {
            let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap();
            let mut folder = store
                .properties()
                .ipm_sub_tree_entry_id()
                .unwrap()
                .node_id();
            folder = find_child_folder(&store, folder, "Inbox").unwrap().unwrap();
            let inbox = store
                .open_folder(&store.properties().make_entry_id(folder).unwrap())
                .unwrap();
            inbox.contents_table().unwrap().clone()
        };
        let subject_cell = || {
            let table = contents();
            let row = table.rows_matrix().next().unwrap();
            let values = row.columns(table.context()).unwrap();
            let (_, value) = table
                .context()
                .columns()
                .iter()
                .zip(values)
                .find(|(column, _)| column.prop_id() == 0x0037)
                .unwrap();
            value
        };
        drop(UnicodePstFile::create(&path, &first).unwrap());
        assert!(
            matches!(subject_cell(), Some(TableRowColumnValue::Heap(id)) if u32::from(id) == 0),
            "after the create"
        );
        drop(
            UnicodePstFile::append(
                &path,
                &UnicodePstMessage {
                    subject: "second",
                    search_key: Some(&key_2),
                    ..first
                },
            )
            .unwrap(),
        );
        assert!(
            matches!(subject_cell(), Some(TableRowColumnValue::Heap(id)) if u32::from(id) == 0),
            "after a later session's append"
        );
        // The rebuild itself, on the same table: the first row's subject cell keeps its existence bit and HNID 0.
        let table = contents();
        let context = table.context();
        let (_, rows, _) =
            rebuild_table_with_rows_and_version(table.as_ref(), Vec::new(), context.rows(), 7)
                .unwrap();
        let column = context
            .columns()
            .iter()
            .find(|column| column.prop_id() == 0x0037)
            .unwrap();
        let row = &rows[0][..usize::from(context.end_existence_bitmap())];
        let bit = usize::from(column.existence_bitmap_index());
        assert_ne!(
            row[usize::from(context.end_1byte_values()) + bit / 8] & (0x80 >> (bit % 8)),
            0,
            "rebuilt: the subject cell is present"
        );
        let offset = usize::from(column.offset());
        assert_eq!(
            u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap()),
            0,
            "rebuilt: HNID 0"
        );
        std::fs::remove_file(path).unwrap();
    }
}
