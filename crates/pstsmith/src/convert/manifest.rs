//! The manifest conversion behind [`convert_manifest`](super::convert_manifest) and the `pstsmith` command line.
//!
//! Input: a `pst-export-manifest/1` JSONL file (header line + one item per line), or a `pst-export-manifest/2` file
//! (header, `folder_count` folder records, then the items).
//! Output: one new PST file, or (manifest v2 only) a new directory holding size-limited parts, `export-0001.pst`,
//! `export-0002.pst` and so on; and the `pst-export-result/1` report. With an exclusion record path (manifest v2
//! only), an item that cannot be written faithfully (a message class that is not converted, or attached e-mails nested
//! deeper than the writer holds) is left out by name and recorded in that new JSONL file, and the rest converts;
//! without it such an item fails the conversion. Errors carry a code, the item key and a short detail, never subjects,
//! bodies, recipient names or file names.
//!
//! Exit codes of the errors: 2 invalid usage/manifest (including an empty selection, and an item the PST format cannot
//! hold: `item-too-large`); 3 source attachment does not match its recorded size/sha256; 4 output path already exists;
//! 5 writer or I/O failure. On any failure after the output was created, the conversion removes what it created: the
//! output files, their directory and the exclusion record.
use crate::{
    guid_bytes, UnicodePstAppendFailed, UnicodePstAttachedMessage, UnicodePstAttachment,
    UnicodePstAttachmentStream, UnicodePstBatchMessage, UnicodePstFileLimitReached,
    UnicodePstImportance, UnicodePstMessage, UnicodePstMessageTooLarge, UnicodePstNamedProperty,
    UnicodePstProperty, UnicodePstPropertyName, UnicodePstRecipient, UnicodePstRecipientType,
    UnicodePstSender, UnicodePstValue, UnicodePstWriter, MESSAGE_OWNED_IDS,
    PS_INTERNET_HEADERS_SET, PS_MAPI_SET, PS_PUBLIC_STRINGS_SET, RECIPIENT_OWNED_IDS,
};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
};

const MANIFEST_SCHEMA: &str = "pst-export-manifest/1";
/// Manifest v2, accepted side by side with v1.
const MANIFEST_SCHEMA_V2: &str = "pst-export-manifest/2";
/// The message classes converted. MS-OXCMSG 2.2.1.3: matching a class is case-insensitive, so
/// `ipm.note` is IPM.Note; the class is written as given. Any other class is left out by name (`--excluded`), never
/// written as mail.
/// Signed and encrypted mail (MS-OXOSMIME 2.1.3). Such a message stores its whole protected content as one file
/// attachment (`smime.p7m`), which the manifest carries like any file, so nothing of it is lost: clear-signed
/// (IPM.Note.SMIME.MultipartSigned), opaque-signed or encrypted (IPM.Note.SMIME), and a secure receipt
/// (IPM.Note.Receipt.SMIME, MS-OXOSMIME product note 9). The attachment and the body are written as given; the structure
/// is not checked, so a mailbox item that breaks 2.1.3 is copied as it is, except that one without a `smime.p7m` file
/// attachment is written as IPM.Note and disclosed (Outlook cannot open it).
const CONVERTED_CLASSES: &[&str] = &[
    "IPM.Note",
    "IPM.Note.SMIME",
    "IPM.Note.SMIME.MultipartSigned",
    "IPM.Note.Receipt.SMIME",
    // Meeting messages (MS-OXOCAL 2.2.6.1, 2.2.7.1, 2.2.8.1 and 2.2.9.1).
    "IPM.Schedule.Meeting.Request",
    "IPM.Schedule.Meeting.Resp.Pos",
    "IPM.Schedule.Meeting.Resp.Tent",
    "IPM.Schedule.Meeting.Resp.Neg",
    "IPM.Schedule.Meeting.Canceled",
    "IPM.Schedule.Meeting.Notification.Forward",
];
/// The meeting classes among CONVERTED_CLASSES.
const MEETING_CLASSES: &[&str] = &[
    "IPM.Schedule.Meeting.Request",
    "IPM.Schedule.Meeting.Resp.Pos",
    "IPM.Schedule.Meeting.Resp.Tent",
    "IPM.Schedule.Meeting.Resp.Neg",
    "IPM.Schedule.Meeting.Canceled",
    "IPM.Schedule.Meeting.Notification.Forward",
];
/// Attached e-mails nest at most this many levels below the item: the writer's own bound. An item holding a deeper one
/// is left out by name.
const MAX_NESTING: usize = 32;
const RESULT_PROTOCOL: &str = "pst-export-result/1";
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");
const WRITER_VERSION: &str = concat!("pstsmith ", env!("CARGO_PKG_VERSION"));
const FILETIME_EPOCH_OFFSET: i64 = 11_644_473_600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    export_id: String,
    item_count: usize,
}

/// A value whose key is required but may be `null` (manifest v2 requires every key; serde alone would read a missing
/// `Option` key as `None`).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

/// A key that may be left out but, when present, is not null (a given property's keys of its other form).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// Reads only the schema of a header line, to choose the parse; unknown keys are left to that parse.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderV2 {
    schema: String,
    export_id: String,
    item_count: usize,
    folder_count: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderRecord {
    folder_path: Vec<String>,
}

/// A manifest v2 item: v1's fields, every key required, plus the message class, the transport headers, attachments
/// that are files or attached e-mails, the draft flag and the creation and modification times, and further properties.
/// The sender, the Message-ID and the received time may be null: a draft has none yet, and null writes no property.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemV2 {
    item_key: String,
    search_key: String,
    #[serde(deserialize_with = "nullable")]
    message_id: Option<String>,
    message_class: String,
    folder_path: Vec<String>,
    subject: String,
    #[serde(deserialize_with = "nullable")]
    sender: Option<Address>,
    to: Vec<RecipientV2>,
    cc: Vec<RecipientV2>,
    bcc: Vec<RecipientV2>,
    body: String,
    #[serde(deserialize_with = "nullable")]
    html: Option<String>,
    #[serde(deserialize_with = "nullable")]
    sent_time: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    received_time: Option<i64>,
    created_time: i64,
    last_modified_time: i64,
    is_draft: bool,
    is_read: bool,
    importance: u8,
    categories: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    transport_headers: Option<String>,
    /// Further properties, tagged or named, written as given.
    properties: Vec<PropertyV2>,
    attachments: Vec<AttachmentV2>,
}

/// One message as the converter writes it: a v2 attached e-mail (an item without `item_key` and `folder_path`), and
/// every item's own message too (a v1 item is converted to this shape).
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct NestedMessage {
    search_key: String,
    #[serde(deserialize_with = "nullable")]
    message_id: Option<String>,
    message_class: String,
    subject: String,
    #[serde(deserialize_with = "nullable")]
    sender: Option<Address>,
    to: Vec<RecipientV2>,
    cc: Vec<RecipientV2>,
    bcc: Vec<RecipientV2>,
    body: String,
    #[serde(deserialize_with = "nullable")]
    html: Option<String>,
    #[serde(deserialize_with = "nullable")]
    sent_time: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    received_time: Option<i64>,
    created_time: i64,
    last_modified_time: i64,
    is_draft: bool,
    is_read: bool,
    importance: u8,
    categories: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    transport_headers: Option<String>,
    properties: Vec<PropertyV2>,
    attachments: Vec<AttachmentV2>,
    /// `properties` as the writer writes them, filled by `validate_message`: the tagged ones and the named ones.
    #[serde(skip)]
    given: Vec<Given>,
    #[serde(skip)]
    named: Vec<GivenNamed>,
}

/// A recipient of a v2 message: its address, and further tagged properties written as given in its row of the
/// recipient table.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct RecipientV2 {
    name: String,
    email: String,
    properties: Vec<PropertyV2>,
    #[serde(skip)]
    given: Vec<Given>,
}

/// A given property in a v2 manifest: tagged, `{"tag": "0x0049", "type": "string", "value": ...}`, or named,
/// `{"set": GUID, "lid": int, ...}` or `{"set": GUID, "name": string, ...}`. The keys of the other form are left out,
/// never null.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct PropertyV2 {
    #[serde(default, deserialize_with = "present")]
    tag: Option<String>,
    #[serde(default, deserialize_with = "present")]
    set: Option<String>,
    #[serde(default, deserialize_with = "present")]
    lid: Option<u32>,
    #[serde(default, deserialize_with = "present")]
    name: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    value: serde_json::Value,
}

impl PropertyV2 {
    /// How an error names the property.
    fn label(&self) -> String {
        match (&self.tag, &self.set, self.lid, &self.name) {
            (Some(tag), ..) => tag.clone(),
            (None, Some(set), Some(lid), _) => format!("{set} LID {lid:#x}"),
            (None, Some(set), None, Some(name)) => format!("{set} name {name:?}"),
            _ => "without a tag or a set".to_owned(),
        }
    }
}

/// A property as the writer writes it.
#[derive(Clone)]
struct Given {
    id: u16,
    value: GivenValue,
}

#[derive(Clone)]
enum GivenValue {
    Integer32(i32),
    Boolean(bool),
    Time(i64),
    Unicode(String),
    Binary(Vec<u8>),
}

/// A named property as the writer writes it.
#[derive(Clone)]
struct GivenNamed {
    set: [u8; 16],
    lid: Option<u32>,
    name: String,
    value: GivenValue,
}

impl GivenValue {
    fn writer(&self) -> UnicodePstValue<'_> {
        match self {
            GivenValue::Integer32(value) => UnicodePstValue::Integer32(*value),
            GivenValue::Boolean(value) => UnicodePstValue::Boolean(*value),
            GivenValue::Time(value) => UnicodePstValue::Time(*value),
            GivenValue::Unicode(value) => UnicodePstValue::Unicode(value),
            GivenValue::Binary(value) => UnicodePstValue::Binary(value),
        }
    }

    /// Bytes a batch holds for it.
    fn held(&self) -> usize {
        match self {
            GivenValue::Unicode(value) => value.len(),
            GivenValue::Binary(value) => value.len(),
            _ => 8,
        }
    }
}

impl GivenNamed {
    fn writer(&self) -> UnicodePstNamedProperty<'_> {
        UnicodePstNamedProperty {
            set: self.set,
            name: self.lid.map_or(
                UnicodePstPropertyName::Name(&self.name),
                UnicodePstPropertyName::Lid,
            ),
            value: self.value.writer(),
        }
    }

    /// The name as a store matches it (MS-OXCPRPT): a LID, or a string, lower-cased in PS_INTERNET_HEADERS.
    fn key(&self) -> ([u8; 16], Option<u32>, String) {
        let name = if self.set == PS_INTERNET_HEADERS_SET {
            self.name.to_ascii_lowercase()
        } else {
            self.name.clone()
        };
        (self.set, self.lid, name)
    }
}

/// A property set's GUID as the manifest writes it, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (hexadecimal, either case), in
/// its stored byte order (MS-OXCDATA 2.5.1).
fn parse_set(text: &str) -> Option<[u8; 16]> {
    let parts = text.split('-').collect::<Vec<_>>();
    let lengths = parts.iter().map(|part| part.len()).collect::<Vec<_>>();
    if lengths != [8, 4, 4, 4, 12]
        || !text
            .bytes()
            .all(|byte| byte == b'-' || byte.is_ascii_hexdigit())
    {
        return None;
    }
    let data1 = u32::from_str_radix(parts[0], 16).ok()?;
    let data2 = u16::from_str_radix(parts[1], 16).ok()?;
    let data3 = u16::from_str_radix(parts[2], 16).ok()?;
    let tail = format!("{}{}", parts[3], parts[4]);
    let mut data4 = [0_u8; 8];
    for (index, byte) in data4.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tail[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(guid_bytes(data1, data2, data3, data4))
}

impl Given {
    fn writer(&self) -> UnicodePstProperty<'_> {
        UnicodePstProperty {
            id: self.id,
            value: self.value.writer(),
        }
    }
}

/// The type of each property MS-OXOMSG 2.2.2 (report messages) and MS-OXCMAIL 2.2.3.7 (their recipients) define, as
/// MS-OXPROPS gives it: such a tag with another type makes the manifest invalid. Other tags take the declared type.
const KNOWN_TYPES: &[(u16, &str)] = &[
    (0x0010, "time"),
    (0x0025, "binary"),
    (0x002A, "time"),
    (0x0031, "binary"),
    (0x0032, "time"),
    (0x003A, "string"),
    (0x0045, "binary"),
    (0x0046, "binary"),
    (0x0049, "string"),
    (0x004D, "string"),
    (0x004E, "time"),
    (0x0053, "binary"),
    (0x0054, "binary"),
    (0x0055, "time"),
    (0x005A, "string"),
    (0x005B, "binary"),
    (0x005C, "binary"),
    (0x005D, "string"),
    (0x005E, "binary"),
    (0x005F, "binary"),
    (0x0066, "string"),
    (0x0067, "string"),
    (0x0068, "string"),
    (0x0069, "string"),
    (0x0072, "string"),
    (0x0073, "string"),
    (0x0074, "string"),
    (0x0080, "string"),
    (0x0081, "string"),
    (0x0C04, "int32"),
    (0x0C05, "int32"),
    (0x0C1B, "string"),
    (0x0C20, "int32"),
    (0x0C21, "string"),
    (0x1001, "string"),
    (0x1039, "string"),
    (0x1046, "string"),
    (0x3A12, "binary"),
    (0x4029, "string"),
    (0x402A, "string"),
    (0x402B, "string"),
    (0x6820, "string"),
    // The tagged properties MS-OXOCAL 2.2.1 to 2.2.12 name, with their MS-OXPROPS types.
    (0x0060, "time"),
    (0x0061, "time"),
    (0x0062, "int32"),
    (0x0063, "bool"),
    (0x0C17, "bool"),
    (0x1080, "int32"),
    (0x3613, "string"),
    (0x36E5, "string"),
    (0x5FE1, "bool"),
    (0x5FE3, "time"),
    (0x5FE4, "time"),
    (0x5FFB, "time"),
    (0x5FFD, "int32"),
    (0x5FFF, "int32"),
    (0x6869, "int32"),
    (0x686A, "binary"),
    (0x686D, "bool"),
    (0x686E, "bool"),
    (0x686F, "bool"),
    (0x7D01, "bool"),
    (0x7FF9, "time"),
    (0x7FFB, "time"),
    (0x7FFC, "time"),
    (0x7FFD, "int32"),
    (0x7FFE, "bool"),
    // The template's contents-table columns that are neither written by the writer nor typed above. A given value of
    // another type fills no cell while the message keeps it, which a check of every contents-table cell against the
    // message (`pst-table-check`) reports after the conversion; typed here, the manifest is refused up front.
    // MS-OXPROPS types where it lists the property; the replication and internal ones (0x0E30-0x0E3D, 0x1097, 0x65C6)
    // at their column's type in the template. 0x0E33 is PtypInteger64, which the manifest cannot give.
    (0x0036, "int32"),
    (0x0057, "bool"),
    (0x0058, "bool"),
    (0x0071, "binary"),
    (0x0E30, "binary"),
    (0x0E33, "int64"),
    (0x0E34, "binary"),
    (0x0E38, "int32"),
    (0x0E3C, "binary"),
    (0x0E3D, "binary"),
    (0x1097, "int32"),
    (0x65C6, "int32"),
];

/// The named properties MS-OXOCAL 2.2.1 to 2.2.9 name, as (set, LID or string name, MS-OXPROPS type): such a named
/// property with another type makes the manifest invalid.
const KNOWN_NAMED_TYPES: &[(&str, Option<u32>, Option<&str>, &str)] = &[
    (
        "00020329-0000-0000-c000-000000000046",
        None,
        Some("DoNotForward"),
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8201),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8202),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8203),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8204),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8205),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8206),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8207),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8208),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8209),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x820A),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x820D),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x820E),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8213),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8214),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8215),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8216),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8217),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8218),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8220),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8223),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8224),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8228),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8229),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x822B),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8230),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8231),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8232),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8233),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8234),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8235),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8236),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8237),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8238),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x823A),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x823B),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x823C),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8240),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8241),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8242),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8243),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8244),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8246),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8247),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8248),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8249),
        None,
        "string",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8250),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8251),
        None,
        "time",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8256),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8257),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8259),
        None,
        "int32",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x825A),
        None,
        "bool",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x825D),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x825E),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x825F),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8260),
        None,
        "binary",
    ),
    (
        "00062002-0000-0000-c000-000000000046",
        Some(0x8261),
        None,
        "binary",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8045),
        None,
        "bool",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8510),
        None,
        "int32",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8516),
        None,
        "time",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8517),
        None,
        "time",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8536),
        None,
        "string",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8537),
        None,
        "string",
    ),
    (
        "00062008-0000-0000-c000-000000000046",
        Some(0x8538),
        None,
        "string",
    ),
    (
        "11000e07-b51b-40d6-af21-caa85edab1d0",
        Some(0x15),
        None,
        "int32",
    ),
    (
        "11000e07-b51b-40d6-af21-caa85edab1d0",
        Some(0x85CC),
        None,
        "bool",
    ),
    (
        "11000e07-b51b-40d6-af21-caa85edab1d0",
        Some(0x85CD),
        None,
        "int32",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x1),
        None,
        "time",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x2),
        None,
        "string",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x3),
        None,
        "binary",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x4),
        None,
        "bool",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x5),
        None,
        "bool",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0xA),
        None,
        "bool",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0xC),
        None,
        "int32",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x1A),
        None,
        "time",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x1C),
        None,
        "int32",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x23),
        None,
        "binary",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x24),
        None,
        "string",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x26),
        None,
        "int32",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x28),
        None,
        "string",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x29),
        None,
        "time",
    ),
    (
        "6ed8da90-450b-101b-98da-00aa003f1305",
        Some(0x2A),
        None,
        "time",
    ),
];

/// A meeting message's identity and time, (set, LID): PidLidGlobalObjectId,
/// PidLidAppointmentStartWhole and PidLidAppointmentEndWhole.
const MEETING_CORE: &[(&str, u32)] = &[
    ("6ed8da90-450b-101b-98da-00aa003f1305", 0x3),
    ("00062002-0000-0000-c000-000000000046", 0x820D),
    ("00062002-0000-0000-c000-000000000046", 0x820E),
];
/// The MS-OXPROPS type of a named property in KNOWN_NAMED_TYPES.
fn known_named_type(set: [u8; 16], lid: Option<u32>, name: &str) -> Option<&'static str> {
    KNOWN_NAMED_TYPES
        .iter()
        .find_map(|(known_set, known_lid, known_name, kind)| {
            let same_name = match lid {
                Some(lid) => *known_lid == Some(lid),
                None => *known_name == Some(name),
            };
            (same_name && parse_set(known_set) == Some(set)).then_some(*kind)
        })
}

/// A message's given named property by (set, LID).
fn named_given<'a>(message: &'a NestedMessage, (set, lid): (&str, u32)) -> Option<&'a GivenNamed> {
    let set = parse_set(set)?;
    message
        .named
        .iter()
        .find(|named| named.set == set && named.lid == Some(lid))
}

/// PidLidAppointmentRecur (PSETID_Appointment LID 0x8216).
const APPOINTMENT_RECUR: (&str, u32) = ("00062002-0000-0000-c000-000000000046", 0x8216);

/// PidLidIsException (PSETID_Meeting LID 0x000A).
const IS_EXCEPTION: (&str, u32) = ("6ed8da90-450b-101b-98da-00aa003f1305", 0x000A);
/// With PidLidAppointmentRecur, the properties that make Outlook look for a recurrence: PidLidRecurring
/// (PSETID_Appointment 0x8223) and PidLidIsRecurring (PSETID_Meeting 0x0005). Classic Outlook cannot open a meeting
/// message whose recurrence BLOB it cannot read ("Cannot locate recurrence information for this appointment"), nor one
/// without the BLOB while PidLidIsRecurring is true, unless it is an exception; it opens one without the three. Without
/// a BLOB it opens an exception (PidLidIsRecurring and PidLidIsException true, PidLidRecurring false: MS-OXOCAL
/// 2.2.1.12, 2.2.1.13) and one with only PidLidRecurring true; it does not open one with PidLidIsRecurring true and
/// PidLidIsException false, nor one with both flags true.
const RECURRENCE_NAMED: [(&str, u32); 3] = [
    APPOINTMENT_RECUR,
    ("00062002-0000-0000-c000-000000000046", 0x8223),
    ("6ed8da90-450b-101b-98da-00aa003f1305", 0x0005),
];

/// The ModifiedInstanceCount of the AppointmentRecurrencePattern that PidLidAppointmentRecur holds (MS-OXOCAL
/// 2.2.1.44.5), read whole: its RecurrencePattern (2.2.1.44.1), then ReaderVersion2 (0x3006), WriterVersion2, the time
/// offsets, ExceptionCount (equal to ModifiedInstanceCount), each ExceptionInfo (2.2.1.44.2), ReservedBlock1, each
/// ExtendedException (2.2.1.44.4, with its ChangeHighlight from WriterVersion2 0x3009) and ReservedBlock2. None when
/// the bytes are not one: a BLOB that is only a RecurrencePattern, or ends before ReservedBlock2, is not readable
/// either (classic Outlook does not open one cut 4 bytes short). Bytes after ReservedBlock2 are left as they are;
/// MFCMAPI reads them as junk data, and classic Outlook opens a pattern with 4 such bytes.
fn recurrence_modified_instances(blob: &[u8]) -> Option<u32> {
    struct Cursor<'a> {
        blob: &'a [u8],
        at: usize,
    }
    impl Cursor<'_> {
        fn skip(&mut self, n: usize) -> Option<()> {
            let end = self.at.checked_add(n)?;
            (end <= self.blob.len()).then(|| self.at = end)
        }
        fn u16(&mut self) -> Option<u16> {
            let bytes = self.blob.get(self.at..self.at.checked_add(2)?)?;
            self.at += 2;
            Some(u16::from_le_bytes([bytes[0], bytes[1]]))
        }
        fn u32(&mut self) -> Option<u32> {
            let bytes = self.blob.get(self.at..self.at.checked_add(4)?)?;
            self.at += 4;
            Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        fn counted(&mut self, unit: usize, count: u32) -> Option<()> {
            self.skip(usize::try_from(count).ok()?.checked_mul(unit)?)
        }
    }
    let mut c = Cursor { blob, at: 0 };
    if c.u16()? != 0x3004 || c.u16()? != 0x3004 {
        return None;
    }
    c.skip(2)?; // RecurFrequency
                // PatternTypeSpecific: none for Day, 4 bytes for Week, Month, MonthEnd, HjMonth and HjMonthEnd, 8 for MonthNth and
                // HjMonthNth.
    let specific = match c.u16()? {
        0x0000 => 0,
        0x0001 | 0x0002 | 0x0004 | 0x000A | 0x000C => 4,
        0x0003 | 0x000B => 8,
        _ => return None,
    };
    c.skip(2 + 4 + 4 + 4 + specific + 4 + 4 + 4)?; // CalendarType .. FirstDOW
    let deleted = c.u32()?;
    c.counted(4, deleted)?;
    let modified = c.u32()?;
    c.counted(4, modified)?;
    c.skip(8)?; // StartDate, EndDate
    if c.u32()? != 0x3006 {
        return None;
    }
    let writer2 = c.u32()?;
    if writer2 < 0x3006 {
        return None;
    }
    c.skip(8)?; // StartTimeOffset, EndTimeOffset
    let count = c.u16()?;
    if u32::from(count) != modified {
        return None;
    }
    let mut flags = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        c.skip(12)?; // StartDateTime, EndDateTime, OriginalStartTime
        let f = c.u16()?;
        flags.push(f);
        // ARO_SUBJECT, then MEETINGTYPE, REMINDERDELTA, REMINDER, then ARO_LOCATION, then BUSYSTATUS, ATTACHMENT,
        // SUBTYPE, APPTCOLOR, in that order.
        for (bit, size) in [
            (0x0001, 0),
            (0x0002, 4),
            (0x0004, 4),
            (0x0008, 4),
            (0x0010, 0),
            (0x0020, 4),
            (0x0040, 4),
            (0x0080, 4),
            (0x0100, 4),
        ] {
            if f & bit == 0 {
                continue;
            }
            if size == 0 {
                let (length, length2) = (c.u16()?, c.u16()?);
                if u32::from(length) != u32::from(length2) + 1 {
                    return None;
                }
                c.skip(usize::from(length2))?;
            } else {
                c.skip(size)?;
            }
        }
    }
    if c.u32()? != 0 {
        return None; // ReservedBlock1Size
    }
    for f in flags {
        if writer2 >= 0x3009 {
            let size = c.u32()?; // ChangeHighlight
            c.counted(1, size)?;
        }
        if c.u32()? != 0 {
            return None; // ReservedBlockEE1Size
        }
        if f & 0x0011 != 0 {
            c.skip(12)?; // StartDateTime, EndDateTime, OriginalStartDate
            if f & 0x0001 != 0 {
                let length = c.u16()?;
                c.counted(2, u32::from(length))?;
            }
            if f & 0x0010 != 0 {
                let length = c.u16()?;
                c.counted(2, u32::from(length))?;
            }
            let size = c.u32()?; // ReservedBlockEE2
            c.counted(1, size)?;
        }
    }
    if c.u32()? != 0 {
        return None; // ReservedBlock2Size
    }
    Some(modified)
}

/// The latest FILETIME a time property may have: 9999-12-31 23:59:59.9999999 UTC, as for the other times.
const MAX_FILETIME: i64 = (MAX_UNIX_SECONDS + FILETIME_EPOCH_OFFSET) * 10_000_000 + 9_999_999;

/// A given property of either form.
enum Parsed {
    Tagged(Given),
    Named(GivenNamed),
}

/// One property's rules. Tagged: a tag "0x" and four hexadecimal digits, from 0x0001 to 0x7FFF, not one the writer
/// writes itself, and for the tags in KNOWN_TYPES their type. Named (messages only): a set GUID with a LID or a
/// non-empty name, not in PS_MAPI (whose LIDs are tags), a header field name in PS_INTERNET_HEADERS, and not the
/// categories' PS_PUBLIC_STRINGS "Keywords". Either: a known type with a value of that type.
fn parse_property(
    key: &str,
    depth: usize,
    what: &str,
    owned: &[u16],
    property: &PropertyV2,
) -> Result<Parsed, Failure> {
    let invalid = |detail: String| {
        Failure::invalid(
            Some(key),
            format!(
                "{what} property {}: {detail} (depth {depth})",
                property.label()
            ),
        )
    };
    let value = || parse_value(property, &invalid);
    let Some(tag) = &property.tag else {
        let (Some(set), true) = (
            &property.set,
            property.lid.is_some() != property.name.is_some(),
        ) else {
            return Err(invalid(
                "a property has a tag, or a set with a lid or a name".to_owned(),
            ));
        };
        if what != "message" {
            return Err(invalid(
                "named properties are written for messages only".to_owned(),
            ));
        }
        let set = parse_set(set).ok_or_else(|| {
            invalid("the set must be a GUID, xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx".to_owned())
        })?;
        if set == PS_MAPI_SET {
            return Err(invalid(
                "a property in PS_MAPI is a tagged property: give its tag".to_owned(),
            ));
        }
        let name = property.name.clone().unwrap_or_default();
        if property.lid.is_none() {
            if name.is_empty() {
                return Err(invalid("the name is empty".to_owned()));
            }
            if set == PS_INTERNET_HEADERS_SET
                && !name
                    .bytes()
                    .all(|byte| (0x21..=0x7E).contains(&byte) && byte != b':')
            {
                return Err(invalid("a PS_INTERNET_HEADERS name is a header field name (RFC 5322: printable ASCII, no colon)".to_owned()));
            }
            if set == PS_PUBLIC_STRINGS_SET && name == "Keywords" {
                return Err(invalid("is the categories (PidNameKeywords), which the converter writes from `categories`".to_owned()));
            }
        }
        // A meeting property has its MS-OXPROPS type.
        if let Some(kind) = known_named_type(set, property.lid, &name) {
            if kind != property.kind {
                return Err(invalid(format!(
                    "has the type {kind} (MS-OXPROPS), not {}",
                    property.kind
                )));
            }
        }
        return Ok(Parsed::Named(GivenNamed {
            set,
            lid: property.lid,
            name,
            value: value()?,
        }));
    };
    if property.set.is_some() || property.lid.is_some() || property.name.is_some() {
        return Err(invalid(
            "a property has a tag, or a set with a lid or a name".to_owned(),
        ));
    }
    let hex = tag
        .strip_prefix("0x")
        .filter(|hex| hex.len() == 4 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid("the tag must be 0x and four hexadecimal digits".to_owned()))?;
    let id = u16::from_str_radix(hex, 16).map_err(|_| invalid("bad tag".to_owned()))?;
    if id == 0 || id >= 0x8000 {
        return Err(invalid("a tag is 0x0001 to 0x7FFF; a named property is given by its set and lid or name, not by an ID".to_owned()));
    }
    if owned.contains(&id) {
        return Err(invalid(
            "is a property the converter writes from its own field".to_owned(),
        ));
    }
    if let Some((_, kind)) = KNOWN_TYPES.iter().find(|(known, _)| *known == id) {
        if *kind != property.kind {
            return Err(invalid(format!(
                "has the type {} (MS-OXPROPS), not {}",
                kind, property.kind
            )));
        }
    }
    Ok(Parsed::Tagged(Given {
        id,
        value: value()?,
    }))
}

/// A given property's value, by its declared type.
fn parse_value(
    property: &PropertyV2,
    invalid: &dyn Fn(String) -> Failure,
) -> Result<GivenValue, Failure> {
    Ok(match (property.kind.as_str(), &property.value) {
        ("int32", serde_json::Value::Number(n)) => GivenValue::Integer32(
            n.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| invalid("is not a 32-bit integer".to_owned()))?,
        ),
        ("bool", serde_json::Value::Bool(value)) => GivenValue::Boolean(*value),
        ("time", serde_json::Value::Number(n)) => GivenValue::Time(
            n.as_i64()
                .filter(|ticks| (0..=MAX_FILETIME).contains(ticks))
                .ok_or_else(|| {
                    invalid("is not a FILETIME from 1601-01-01 to 9999-12-31".to_owned())
                })?,
        ),
        ("string", serde_json::Value::String(value)) => GivenValue::Unicode(value.clone()),
        ("binary", serde_json::Value::String(hex)) => {
            if hex.len() % 2 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(invalid(
                    "binary must be an even number of hexadecimal digits".to_owned(),
                ));
            }
            GivenValue::Binary(
                (0..hex.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0))
                    .collect(),
            )
        }
        ("int32" | "bool" | "time" | "string" | "binary", _) => {
            return Err(invalid(format!("the value is not a {}", property.kind)))
        }
        (other, _) => {
            return Err(invalid(format!(
                "unknown type {other:?} (int32, bool, time, string or binary)"
            )))
        }
    })
}

/// All properties of one list: each by `parse_property`, none twice (a named one by its name as a store matches it).
fn parse_properties(
    key: &str,
    depth: usize,
    what: &str,
    owned: &[u16],
    properties: &[PropertyV2],
) -> Result<(Vec<Given>, Vec<GivenNamed>), Failure> {
    let mut given: Vec<Given> = Vec::with_capacity(properties.len());
    let mut named: Vec<GivenNamed> = Vec::new();
    let twice = |property: &PropertyV2| {
        Failure::invalid(
            Some(key),
            format!(
                "{what} property {} is given twice (depth {depth})",
                property.label()
            ),
        )
    };
    for property in properties {
        match parse_property(key, depth, what, owned, property)? {
            Parsed::Tagged(parsed) => {
                if given.iter().any(|other| other.id == parsed.id) {
                    return Err(twice(property));
                }
                given.push(parsed);
            }
            Parsed::Named(parsed) => {
                if named.iter().any(|other| other.key() == parsed.key()) {
                    return Err(twice(property));
                }
                named.push(parsed);
            }
        }
    }
    Ok((given, named))
}

#[derive(Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum AttachmentV2 {
    File {
        path: String,
        name: String,
        mime: String,
        #[serde(deserialize_with = "nullable")]
        content_id: Option<String>,
        inline: bool,
        size: u64,
        sha256: String,
    },
    Message {
        name: String,
        inline: bool,
        message: Box<NestedMessage>,
    },
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Address {
    name: String,
    email: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    path: String,
    name: String,
    mime: String,
    content_id: Option<String>,
    inline: bool,
    size: u64,
    sha256: String,
}

/// A manifest v1 item line, unchanged.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemV1 {
    item_key: String,
    search_key: String,
    message_id: String,
    folder_path: Vec<String>,
    subject: String,
    sender: Address,
    to: Vec<Address>,
    cc: Vec<Address>,
    bcc: Vec<Address>,
    body: String,
    html: Option<String>,
    sent_time: Option<i64>,
    received_time: i64,
    is_read: bool,
    importance: u8,
    categories: Vec<String>,
    attachments: Vec<Attachment>,
}

/// An item to convert: its folder and its message.
struct Item {
    item_key: String,
    folder_path: Vec<String>,
    message: NestedMessage,
    /// Read from a v2 manifest, where a `message/rfc822` file is a file: v2 says an attached e-mail with `kind`.
    manifest_v2: bool,
}

impl From<Address> for RecipientV2 {
    /// A v1 recipient: an address, without further properties.
    fn from(address: Address) -> Self {
        Self {
            name: address.name,
            email: address.email,
            properties: Vec::new(),
            given: Vec::new(),
        }
    }
}

/// A v1 item as the converter's item. v1 has no class, no draft flag and no creation or modification time: the class
/// is IPM.Note, and both times are the received time.
fn item_from_v1(item: ItemV1) -> Item {
    Item {
        item_key: item.item_key,
        folder_path: item.folder_path,
        message: NestedMessage {
            search_key: item.search_key,
            message_id: Some(item.message_id),
            message_class: "IPM.Note".to_owned(),
            subject: item.subject,
            sender: Some(item.sender),
            to: item.to.into_iter().map(RecipientV2::from).collect(),
            cc: item.cc.into_iter().map(RecipientV2::from).collect(),
            bcc: item.bcc.into_iter().map(RecipientV2::from).collect(),
            body: item.body,
            html: item.html,
            sent_time: item.sent_time,
            received_time: Some(item.received_time),
            created_time: item.received_time,
            last_modified_time: item.received_time,
            is_draft: false,
            is_read: item.is_read,
            importance: item.importance,
            categories: item.categories,
            transport_headers: None,
            properties: Vec::new(),
            given: Vec::new(),
            named: Vec::new(),
            attachments: item
                .attachments
                .into_iter()
                .map(|a| AttachmentV2::File {
                    path: a.path,
                    name: a.name,
                    mime: a.mime,
                    content_id: a.content_id,
                    inline: a.inline,
                    size: a.size,
                    sha256: a.sha256,
                })
                .collect(),
        },
        manifest_v2: false,
    }
}

pub(crate) struct Failure {
    pub(crate) code: &'static str,
    pub(crate) exit: u8,
    pub(crate) item_key: Option<String>,
    pub(crate) detail: String,
}

impl Failure {
    fn new(
        code: &'static str,
        exit: u8,
        item_key: Option<&str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            exit,
            item_key: item_key.map(str::to_owned),
            detail: detail.into(),
        }
    }
    fn invalid(item_key: Option<&str>, detail: impl Into<String>) -> Self {
        Self::new("invalid-manifest", 2, item_key, detail)
    }
}

/// A writer error of one commit over `rows[members]`. A message the PST format cannot hold (its size does not fit
/// PidTagMessageSize or PidTagAttachSize, 32-bit) is `item-too-large`, exit 2, with that item's key: the input cannot
/// be converted, not the writer failed. Everything else stays `writer`, exit 5. Its item key is the item whose message
/// the writer was preparing or writing when it failed (out of space, for example); none when the failure is the
/// commit's or the file's, not one message's.
fn writer_failure(error: io::Error, rows: &[Item], members: &[usize]) -> Failure {
    if let Some(too_large) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<UnicodePstMessageTooLarge>())
    {
        let index = too_large
            .position
            .checked_sub(1)
            .and_then(|at| members.get(at))
            .copied()
            .unwrap_or(members[0]);
        return Failure::new(
            "item-too-large",
            2,
            Some(&rows[index].item_key),
            format!(
                "{} bytes do not fit the PST format's 32-bit message and attachment sizes",
                too_large.size
            ),
        );
    }
    let at = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<UnicodePstAppendFailed>())
        .and_then(|failed| failed.position.checked_sub(1))
        .and_then(|at| members.get(at));
    Failure::new(
        "writer",
        5,
        at.map(|&index| rows[index].item_key.as_str()),
        error.to_string(),
    )
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::new("io", 5, None, format!("{}: {}", error.kind(), error))
    }
}

/// Fields the writer cannot represent. Each counter is incremented only when the input
/// value differs from what the writer will produce, so a zero means "nothing was lost".
#[derive(Default)]
struct Unsupported {
    counts: BTreeMap<&'static str, usize>,
}

impl Unsupported {
    fn note(&mut self, field: &'static str) {
        *self.counts.entry(field).or_default() += 1;
    }
}

pub(crate) struct Options {
    pub(crate) manifest: PathBuf,
    pub(crate) output: Target,
    /// `--max-file-size` (manifest v2 only): one of FILE_SIZE_CHOICES; `None` takes the default. With the feature
    /// `test-file-sizes` (tests only) `--max-file-bytes` sets any size.
    pub(crate) file_size: Option<u64>,
    /// Where items left out are recorded, one JSON line each. Without it, such an item fails the conversion.
    pub(crate) excluded: Option<PathBuf>,
    pub(crate) batch_items: usize,
    pub(crate) batch_bytes: usize,
    /// Once true, the conversion stops before its next item with `interrupted`, and what it created is removed.
    pub(crate) cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

fn decode_search_key(item: &ItemV1) -> Result<[u8; 16], Failure> {
    let hex = item.search_key.as_bytes();
    if hex.len() != 32 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return Err(Failure::invalid(
            Some(&item.item_key),
            "search_key must be 32 hexadecimal characters (16 bytes)",
        ));
    }
    let mut key = [0u8; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&item.search_key[index * 2..index * 2 + 2], 16).unwrap_or(0);
    }
    Ok(key)
}

/// The v1 rules of an item, unchanged.
/// A folder path's names: none empty, and none longer than the writer stores in a new folder's property context, one
/// heap allocation of 3,580 bytes (MS-PST 2.3.1): 1,790 UTF-16 code units. Exchange's own limit is
/// 250 characters (MS-STANXIMAP C0004).
fn folder_path_problem(folder_path: &[String]) -> Option<&'static str> {
    if folder_path.is_empty() || folder_path.iter().any(String::is_empty) {
        Some("folder_path must be a non-empty list of non-empty names")
    } else if folder_path
        .iter()
        .any(|name| name.encode_utf16().count() > 1_790)
    {
        Some("a folder name is limited to 1,790 UTF-16 code units (one heap allocation, MS-PST 2.3.1)")
    } else {
        None
    }
}

fn validate_item(item: &ItemV1) -> Result<(), Failure> {
    let key = Some(item.item_key.as_str());
    if item.item_key.is_empty() {
        return Err(Failure::invalid(None, "item_key must not be empty"));
    }
    if let Some(problem) = folder_path_problem(&item.folder_path) {
        return Err(Failure::invalid(key, problem));
    }
    if item.importance > 2 {
        return Err(Failure::invalid(key, "importance must be 0, 1 or 2"));
    }
    if item.message_id.is_empty() {
        return Err(Failure::invalid(key, "message_id must not be empty"));
    }
    for (index, attachment) in item.attachments.iter().enumerate() {
        if attachment.name.is_empty() || attachment.sha256.len() != 64 {
            return Err(Failure::invalid(
                key,
                format!("attachment {index} needs a name and a 64-character sha256"),
            ));
        }
        if attachment.inline && attachment.content_id.is_none() {
            return Err(Failure::invalid(
                key,
                format!("attachment {index} is inline without content_id"),
            ));
        }
    }
    decode_search_key(item).map(|_| ())
}

fn valid_search_key(hex: &str) -> bool {
    hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// MS-OXCMSG 2.2.1.3: a message class is 1 to 255 characters, all ASCII 0x20 through 0x7F, not ending with a period.
fn valid_message_class(class: &str) -> bool {
    !class.is_empty()
        && class.len() <= 255
        && !class.ends_with('.')
        && class.bytes().all(|byte| (0x20..=0x7F).contains(&byte))
}

/// The v2 rules of one message, the item's own or an attached one at `depth` (0 = the item), and of its attachments,
/// at every depth. A message the converter does not write is still a well-formed one: these rules hold for it too.
fn validate_message(key: &str, depth: usize, message: &mut NestedMessage) -> Result<(), Failure> {
    // The given properties, of the message and of each recipient; one recipient tag, one type.
    (message.given, message.named) = parse_properties(
        key,
        depth,
        "message",
        MESSAGE_OWNED_IDS,
        &message.properties,
    )?;
    let mut recipient_types: BTreeMap<u16, String> = BTreeMap::new();
    for recipient in message
        .to
        .iter_mut()
        .chain(message.cc.iter_mut())
        .chain(message.bcc.iter_mut())
    {
        // A recipient's named property is refused by `parse_property`, so the second list is always empty.
        (recipient.given, _) = parse_properties(
            key,
            depth,
            "recipient",
            RECIPIENT_OWNED_IDS,
            &recipient.properties,
        )?;
        for (given, property) in recipient.given.iter().zip(&recipient.properties) {
            if recipient_types
                .entry(given.id)
                .or_insert_with(|| property.kind.clone())
                != &property.kind
            {
                return Err(Failure::invalid(
                    Some(key),
                    format!(
                        "recipient property {} has two types in one message (depth {depth})",
                        property.label()
                    ),
                ));
            }
        }
    }
    if !valid_message_class(&message.message_class) {
        return Err(Failure::invalid(Some(key), format!("message_class must be 1 to 255 ASCII characters from 0x20 to 0x7F, not ending with a period (MS-OXCMSG 2.2.1.3) (depth {depth})")));
    }
    if message.importance > 2 {
        return Err(Failure::invalid(
            Some(key),
            format!("importance must be 0, 1 or 2 (depth {depth})"),
        ));
    }
    if message.message_id.as_deref() == Some("") {
        return Err(Failure::invalid(
            Some(key),
            format!("message_id must be null or non-empty (depth {depth})"),
        ));
    }
    if !valid_search_key(&message.search_key) {
        return Err(Failure::invalid(
            Some(key),
            format!("search_key must be 32 hexadecimal characters (16 bytes) (depth {depth})"),
        ));
    }
    // A draft has not been sent (MS-OXCMSG 2.2.1.6, mfUnsent), so it has no sent time.
    if message.is_draft && message.sent_time.is_some() {
        return Err(Failure::invalid(
            Some(key),
            format!("a draft has no sent_time (depth {depth})"),
        ));
    }
    // Every time is range-checked here, for an item that is then left out too: an excluded item never reaches
    // `collect`, and manifest v2 makes such a time `invalid-manifest`.
    for (field, value) in [
        ("received_time", message.received_time),
        ("sent_time", message.sent_time),
        ("created_time", Some(message.created_time)),
        ("last_modified_time", Some(message.last_modified_time)),
    ] {
        if let Some(value) = value {
            if filetime(field, value).is_err() {
                return Err(Failure::invalid(Some(key), format!("{field} is outside the supported range 1601-01-01..9999-12-31 (depth {depth})")));
            }
        }
    }
    for (index, attachment) in message.attachments.iter_mut().enumerate() {
        match attachment {
            AttachmentV2::File {
                name,
                sha256,
                inline,
                content_id,
                ..
            } => {
                if name.is_empty() || sha256.len() != 64 {
                    return Err(Failure::invalid(Some(key), format!("attachment {index} needs a name and a 64-character sha256 (depth {depth})")));
                }
                if *inline && content_id.is_none() {
                    return Err(Failure::invalid(
                        Some(key),
                        format!("attachment {index} is inline without content_id (depth {depth})"),
                    ));
                }
                // The writer stores a Content-ID without one pair of angle brackets (MAPI's form) and
                // refuses an empty one; an empty one ("" or "<>") is the manifest's error, not the writer's.
                if let Some(id) = content_id.as_deref() {
                    let bare = id.strip_prefix('<').unwrap_or(id);
                    if bare.strip_suffix('>').unwrap_or(bare).is_empty() {
                        return Err(Failure::invalid(
                            Some(key),
                            format!("attachment {index} has an empty content_id (depth {depth})"),
                        ));
                    }
                }
            }
            AttachmentV2::Message {
                name,
                inline,
                message,
            } => {
                if name.is_empty() {
                    return Err(Failure::invalid(
                        Some(key),
                        format!("attachment {index} needs a name (depth {depth})"),
                    ));
                }
                // An attached e-mail has no Content-ID to be referenced by, so it cannot be inline.
                if *inline {
                    return Err(Failure::invalid(Some(key), format!("attachment {index} is an attached e-mail marked inline (depth {depth})")));
                }
                validate_message(key, depth + 1, message)?;
            }
        }
    }
    Ok(())
}

/// Why an item is left out: the first message, in depth-first order (a message before the e-mails attached
/// to it, attachments in order), that sits deeper than `MAX_NESTING` or has a class the converter does not convert.
/// `at` is the attachment positions leading to it, empty for the item's own message.
struct Exclusion {
    reason: &'static str,
    message_class: Option<String>,
    at: Vec<usize>,
}

impl Exclusion {
    /// The disclosure counter of this reason, in the result's `unsupported`.
    fn disclosure(&self) -> &'static str {
        if self.reason == "nesting-too-deep" {
            "excluded_nesting_depth"
        } else {
            "excluded_item_class"
        }
    }
}

fn exclusion(message: &NestedMessage, depth: usize, at: &mut Vec<usize>) -> Option<Exclusion> {
    if depth > MAX_NESTING {
        return Some(Exclusion {
            reason: "nesting-too-deep",
            message_class: None,
            at: at.clone(),
        });
    }
    if !converted_class(&message.message_class) {
        return Some(Exclusion {
            reason: "unsupported-item-class",
            message_class: Some(message.message_class.clone()),
            at: at.clone(),
        });
    }
    for (position, attachment) in message.attachments.iter().enumerate() {
        if let AttachmentV2::Message { message, .. } = attachment {
            at.push(position);
            if let Some(found) = exclusion(message, depth + 1, at) {
                return Some(found);
            }
            at.pop();
        }
    }
    None
}

/// A v2 item as the converter's item, after the item rules and the message rules at every depth.
fn item_from_v2(item: ItemV2) -> Result<Item, Failure> {
    if item.item_key.is_empty() {
        return Err(Failure::invalid(None, "item_key must not be empty"));
    }
    if let Some(problem) = folder_path_problem(&item.folder_path) {
        return Err(Failure::invalid(Some(&item.item_key), problem));
    }
    let message = NestedMessage {
        search_key: item.search_key,
        message_id: item.message_id,
        message_class: item.message_class,
        subject: item.subject,
        sender: item.sender,
        to: item.to,
        cc: item.cc,
        bcc: item.bcc,
        body: item.body,
        html: item.html,
        sent_time: item.sent_time,
        received_time: item.received_time,
        created_time: item.created_time,
        last_modified_time: item.last_modified_time,
        is_draft: item.is_draft,
        is_read: item.is_read,
        importance: item.importance,
        categories: item.categories,
        transport_headers: item.transport_headers,
        properties: item.properties,
        given: Vec::new(),
        named: Vec::new(),
        attachments: item.attachments,
    };
    let mut message = message;
    validate_message(&item.item_key, 0, &mut message)?;
    Ok(Item {
        item_key: item.item_key,
        folder_path: item.folder_path,
        message,
        manifest_v2: true,
    })
}

/// The first source problem an attachment stream met during a commit. The writer sees only an I/O error and fails
/// the commit; the converter then reports the source problem with its own code and exit status 3.
type SourceFailure = Rc<RefCell<Option<Failure>>>;

/// One attachment read once, while the writer writes it. Every byte the writer consumes
/// passes through SHA-256 here; size and digest are checked when the stream ends, and the writer requires that end
/// right after the declared length. So the bytes written are exactly the bytes verified, and the pathname is opened
/// once per write and never reopened. Nothing of the attachment is held beyond the writer's block buffer.
struct VerifiedAttachment {
    file: File,
    hasher: Sha256,
    seen: u64,
    size: u64,
    sha256: String,
    /// The attachment's position: its index, or for a file inside attached e-mails the dotted path of positions.
    label: String,
    item_key: String,
    failure: SourceFailure,
    verified: bool,
}

impl VerifiedAttachment {
    fn fail(&self, code: &'static str, detail: String) -> io::Error {
        let mut failure = self.failure.borrow_mut();
        if failure.is_none() {
            *failure = Some(Failure::new(code, 3, Some(&self.item_key), detail.clone()));
        }
        io::Error::new(io::ErrorKind::InvalidData, detail)
    }
}

impl VerifiedAttachment {
    /// At the recorded size: the source must end there and the digest must match. Checked here, not only when the
    /// writer asks for the end of the stream, so the check does not depend on how the writer reads.
    fn finish(&mut self) -> io::Result<()> {
        let mut probe = [0_u8; 1];
        loop {
            match self.file.read(&mut probe) {
                Ok(0) => break,
                Ok(_) => {
                    return Err(self.fail(
                        "source-mismatch",
                        format!(
                            "attachment {}: more bytes than the recorded {}",
                            self.label, self.size
                        ),
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(self.fail(
                        "source-unreadable",
                        format!("attachment {}: {}", self.label, error.kind()),
                    ))
                }
            }
        }
        let digest = format!("{:x}", self.hasher.clone().finalize());
        if !digest.eq_ignore_ascii_case(&self.sha256) {
            return Err(self.fail(
                "source-mismatch",
                format!(
                    "attachment {}: sha256 differs from recorded value",
                    self.label
                ),
            ));
        }
        self.verified = true;
        Ok(())
    }
}

impl Read for VerifiedAttachment {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.verified {
            return Ok(0);
        }
        // A stop ends the commit that is writing this attachment: the writer cuts back to its last commit.
        if stop_requested() {
            let mut failure = self.failure.borrow_mut();
            if failure.is_none() {
                *failure = Some(stopped());
            }
            return Err(io::Error::other("the conversion was stopped"));
        }
        if self.seen == self.size {
            self.finish()?;
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(self.size - self.seen).unwrap_or(usize::MAX));
        let read = match self.file.read(&mut buf[..want]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
            Err(error) => {
                return Err(self.fail(
                    "source-unreadable",
                    format!("attachment {}: {}", self.label, error.kind()),
                ))
            }
        };
        if read == 0 {
            // The source ended before the recorded size (reads never ask past it).
            return Err(self.fail(
                "source-mismatch",
                format!(
                    "attachment {}: size {} differs from recorded {}",
                    self.label, self.seen, self.size
                ),
            ));
        }
        self.seen += read as u64;
        self.hasher.update(&buf[..read]);
        if self.seen == self.size {
            self.finish()?;
        }
        Ok(read)
    }
}

type Opener = Box<dyn Fn() -> io::Result<Box<dyn Read>>>;

/// The writer's one-shot source for an attachment: opens the file and verifies while it is read (see above).
fn attachment_opener(
    item_key: &str,
    label: &str,
    attachment: &Attachment,
    failure: &SourceFailure,
) -> Opener {
    let (path, size, sha256, item_key, label, failure) = (
        attachment.path.clone(),
        attachment.size,
        attachment.sha256.clone(),
        item_key.to_owned(),
        label.to_owned(),
        failure.clone(),
    );
    Box::new(move || match File::open(&path) {
        Ok(file) => Ok(Box::new(VerifiedAttachment {
            file,
            hasher: Sha256::new(),
            seen: 0,
            size,
            sha256: sha256.clone(),
            label: label.clone(),
            item_key: item_key.clone(),
            failure: failure.clone(),
            verified: false,
        })),
        Err(error) => {
            let mut record = failure.borrow_mut();
            if record.is_none() {
                *record = Some(Failure::new(
                    "source-unreadable",
                    3,
                    Some(&item_key),
                    format!("attachment {label}: {}", error.kind()),
                ));
            }
            Err(error)
        }
    })
}

/// An attachment recorded as empty: the writer writes no bytes for it, so it is checked here (it must open, end at
/// once and have the digest of no bytes).
fn verify_empty_attachment(
    item_key: &str,
    label: &str,
    attachment: &Attachment,
) -> Result<(), Failure> {
    let failure: SourceFailure = Rc::new(RefCell::new(None));
    let mut reader = attachment_opener(item_key, label, attachment, &failure)().map_err(|_| {
        failure.borrow_mut().take().unwrap_or_else(|| {
            Failure::new(
                "source-unreadable",
                3,
                Some(item_key),
                format!("attachment {label}"),
            )
        })
    })?;
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).map_err(|_| {
        failure.borrow_mut().take().unwrap_or_else(|| {
            Failure::new(
                "source-mismatch",
                3,
                Some(item_key),
                format!("attachment {label}"),
            )
        })
    })?;
    Ok(())
}

/// The opener of one file, or none for an empty one, which is verified here (the writer never opens it).
fn file_opener(
    item_key: &str,
    label: &str,
    attachment: &Attachment,
    failure: &SourceFailure,
) -> Result<Option<Opener>, Failure> {
    if attachment.size == 0 {
        verify_empty_attachment(item_key, label, attachment)?;
        Ok(None)
    } else {
        Ok(Some(attachment_opener(
            item_key, label, attachment, failure,
        )))
    }
}

/// What a message borrows while the writer writes it: an item's own message, or an attached e-mail. Every message of a
/// batch gets one, item by item and depth first, before any message refers to it.
struct MessageStore<'a> {
    message: &'a NestedMessage,
    /// The class written: the given one, or IPM.Note for an S/MIME message without its protected content.
    class: &'a str,
    /// The message's given properties, and each recipient's, in the order to, cc, bcc.
    given: Vec<UnicodePstProperty<'a>>,
    named: Vec<UnicodePstNamedProperty<'a>>,
    recipient_given: Vec<Vec<UnicodePstProperty<'a>>>,
    categories: Vec<&'a str>,
    search_key: [u8; 16],
    delivery_time: Option<i64>,
    submit_time: Option<i64>,
    creation_time: i64,
    modification_time: i64,
    /// Per attachment of the message, in input order: the opener of a non-empty file, else none.
    openers: Vec<Option<Opener>>,
}

fn recipients_of(
    message: &NestedMessage,
) -> impl Iterator<Item = (&RecipientV2, UnicodePstRecipientType)> {
    message
        .to
        .iter()
        .map(|r| (r, UnicodePstRecipientType::To))
        .chain(message.cc.iter().map(|r| (r, UnicodePstRecipientType::Cc)))
        .chain(
            message
                .bcc
                .iter()
                .map(|r| (r, UnicodePstRecipientType::Bcc)),
        )
}

/// Every store's recipients for the writer, built once the stores exist (a recipient borrows its given properties from
/// its store).
fn recipient_lists<'a>(stores: &'a [MessageStore<'a>]) -> Vec<Vec<UnicodePstRecipient<'a>>> {
    stores
        .iter()
        .map(|store| {
            recipients_of(store.message)
                .zip(&store.recipient_given)
                .map(|((r, recipient_type), properties)| UnicodePstRecipient {
                    name: &r.name,
                    email: &r.email,
                    recipient_type,
                    properties,
                })
                .collect()
        })
        .collect()
}

/// The class suffixes of report messages (MS-OXCMAIL 2.2.3.7.1.1 for delivery status, 2.2.3.7.2 for message
/// disposition), longest first so that ".Expanded.DR" is not read as ".DR".
const REPORT_SUFFIXES: &[&str] = &[
    ".Expanded.DR",
    ".Relayed.DR",
    ".Delayed.DR",
    ".IPNNRN",
    ".IPNRN",
    ".NDR",
    ".DR",
];

/// (the original class, the suffix) of a report class, "REPORT." + class + suffix, matched case-insensitively.
fn report_of(class: &str) -> Option<(&str, &'static str)> {
    if class.len() < 7 || !class[..7].eq_ignore_ascii_case("REPORT.") {
        return None;
    }
    REPORT_SUFFIXES.iter().find_map(|suffix| {
        let cut = class
            .len()
            .checked_sub(suffix.len())
            .filter(|cut| *cut > 7)?;
        class[cut..]
            .eq_ignore_ascii_case(suffix)
            .then(|| (&class[7..cut], *suffix))
    })
}

/// Whether a class is written: one of CONVERTED_CLASSES, or a report about one of them.
fn converted_class(class: &str) -> bool {
    let base = |class: &str| {
        CONVERTED_CLASSES
            .iter()
            .any(|converted| converted.eq_ignore_ascii_case(class))
    };
    base(class) || report_of(class).is_some_and(|(original, _)| base(original))
}

/// The recipient property that carries a delivery status report's status (MS-OXCMAIL 2.2.3.7.1): a non-delivery
/// report's PidTagNonDeliveryReportStatusCode, a delivery report's PidTagDeliverTime. None for other classes.
fn report_status_tag(class: &str) -> Option<u16> {
    match report_of(class)?.1 {
        ".NDR" => Some(0x0C20),
        ".IPNRN" | ".IPNNRN" => None,
        _ => Some(0x0010),
    }
}

fn search_key_bytes(hex: &str, item_key: &str) -> Result<[u8; 16], Failure> {
    if !valid_search_key(hex) {
        return Err(Failure::invalid(
            Some(item_key),
            "search_key must be 32 hexadecimal characters (16 bytes)",
        ));
    }
    let mut key = [0u8; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or(0);
    }
    Ok(key)
}

/// Depth first: the message's store, then those of the e-mails attached to it, in input order. `label` is the dotted
/// position of the attachment that holds the message, `None` for an item's own message; a file's label is its position
/// among the message's attachments (for v1, whose attachments are all files, that is its index), prefixed by `label`.
fn collect<'a>(
    item_key: &str,
    label: Option<&str>,
    message: &'a NestedMessage,
    stores: &mut Vec<MessageStore<'a>>,
    failure: &SourceFailure,
    unsupported: &mut Unsupported,
) -> Result<(), Failure> {
    let with_key = |mut failure: Failure| {
        failure.item_key = Some(item_key.to_owned());
        failure
    };
    let position_label = |position: usize| {
        label.map_or_else(
            || position.to_string(),
            |label| format!("{label}.{position}"),
        )
    };
    let mut openers = Vec::with_capacity(message.attachments.len());
    for (position, attachment) in message.attachments.iter().enumerate() {
        openers.push(match attachment {
            AttachmentV2::File {
                path,
                name,
                mime,
                content_id,
                inline,
                size,
                sha256,
            } => {
                let file = Attachment {
                    path: path.clone(),
                    name: name.clone(),
                    mime: mime.clone(),
                    content_id: content_id.clone(),
                    inline: *inline,
                    size: *size,
                    sha256: sha256.clone(),
                };
                file_opener(item_key, &position_label(position), &file, failure)?
            }
            AttachmentV2::Message { .. } => None,
        });
    }
    // A zero-length PR_HTML written by this writer is not readable by an independent .NET reader, so present-but-empty
    // HTML is written as absent and disclosed, at every depth.
    if message.html.as_deref() == Some("") {
        unsupported.note("empty_html_treated_as_absent");
    }
    // A delivery status report whose recipients do not all carry their status (Graph's object model has no
    // recipient-level properties) is written, and that loss is disclosed per message.
    if let Some(tag) = report_status_tag(&message.message_class) {
        let mut recipients = recipients_of(message).peekable();
        if recipients.peek().is_none()
            || recipients.any(|(r, _)| !r.given.iter().any(|given| given.id == tag))
        {
            unsupported.note("report_recipient_status");
        }
    }
    // A meeting message without its identity and time, and one whose recurrence has modified instances, are written,
    // and those losses disclosed per message. One whose recurrence cannot be read is written without
    // PidLidAppointmentRecur, PidLidRecurring and PidLidIsRecurring (Outlook cannot open it with them), and that loss
    // disclosed (`meeting_recurrence_unreadable`).
    let mut omit_recurrence = false;
    let is_true = |flag| {
        matches!(
            named_given(message, flag).map(|named| &named.value),
            Some(GivenValue::Boolean(true))
        )
    };
    if MEETING_CLASSES
        .iter()
        .any(|class| class.eq_ignore_ascii_case(&message.message_class))
    {
        if MEETING_CORE
            .iter()
            .any(|core| named_given(message, *core).is_none())
        {
            unsupported.note("meeting_core_properties");
        }
        if let Some(recur) = named_given(message, APPOINTMENT_RECUR) {
            let modified = match &recur.value {
                GivenValue::Binary(blob) => recurrence_modified_instances(blob),
                _ => None,
            };
            match modified {
                None => {
                    unsupported.note("meeting_recurrence_unreadable");
                    omit_recurrence = true;
                }
                Some(0) => {}
                Some(_) => unsupported.note("meeting_exception_attachments"),
            }
        } else if is_true(RECURRENCE_NAMED[2])
            && !(is_true(IS_EXCEPTION) && !is_true(RECURRENCE_NAMED[1]))
        {
            // No BLOB while PidLidIsRecurring is true. Outlook cannot open that either, so the flags are left out and
            // the loss disclosed the same way. Not for an exception (PidLidIsException true, PidLidRecurring not;
            // MS-OXOCAL 2.2.1.12, 2.2.1.13), nor with only PidLidRecurring true: classic Outlook opens both as given.
            unsupported.note("meeting_recurrence_unreadable");
            omit_recurrence = true;
        }
    }
    // An S/MIME message's protected content is its `smime.p7m` file attachment (MS-OXOSMIME 2.1.3). Without one,
    // classic Outlook cannot open the message ("An error occurred in the underlying security system"); it is written
    // as IPM.Note with everything given, and disclosed (`smime_protected_content_missing`).
    let smime_without_content = ["IPM.Note.SMIME", "IPM.Note.SMIME.MultipartSigned", "IPM.Note.Receipt.SMIME"]
        .iter()
        .any(|class| class.eq_ignore_ascii_case(&message.message_class))
        && !message.attachments.iter().any(|attachment| matches!(attachment, AttachmentV2::File { name, .. } if name.eq_ignore_ascii_case("smime.p7m")));
    if smime_without_content {
        unsupported.note("smime_protected_content_missing");
    }
    let omitted: Vec<&GivenNamed> = if omit_recurrence {
        RECURRENCE_NAMED
            .iter()
            .filter_map(|named| named_given(message, *named))
            .collect()
    } else {
        Vec::new()
    };
    stores.push(MessageStore {
        message,
        class: if smime_without_content {
            "IPM.Note"
        } else {
            &message.message_class
        },
        given: message.given.iter().map(Given::writer).collect(),
        named: message
            .named
            .iter()
            .filter(|named| !omitted.iter().any(|left| std::ptr::eq(*left, *named)))
            .map(GivenNamed::writer)
            .collect(),
        recipient_given: recipients_of(message)
            .map(|(r, _)| r.given.iter().map(Given::writer).collect())
            .collect(),
        categories: message.categories.iter().map(String::as_str).collect(),
        search_key: search_key_bytes(&message.search_key, item_key)?,
        delivery_time: message
            .received_time
            .map(|received| filetime("received_time", received))
            .transpose()
            .map_err(with_key)?,
        submit_time: message
            .sent_time
            .map(|sent| filetime("sent_time", sent))
            .transpose()
            .map_err(with_key)?,
        creation_time: filetime("created_time", message.created_time).map_err(with_key)?,
        modification_time: filetime("last_modified_time", message.last_modified_time)
            .map_err(with_key)?,
        openers,
    });
    for (position, attachment) in message.attachments.iter().enumerate() {
        if let AttachmentV2::Message { message, .. } = attachment {
            collect(
                item_key,
                Some(&position_label(position)),
                message,
                stores,
                failure,
                unsupported,
            )?;
        }
    }
    Ok(())
}

/// A message as the writer's message and attachments; it takes the next store, and the e-mails attached to it the ones
/// after, in the order `collect` made them.
fn writer_parts<'a>(
    message: &'a NestedMessage,
    stores: &'a [MessageStore<'a>],
    recipients: &'a [Vec<UnicodePstRecipient<'a>>],
    next: &mut usize,
) -> (UnicodePstMessage<'a>, Vec<UnicodePstAttachment<'a>>) {
    let store = &stores[*next];
    let store_recipients = &recipients[*next];
    *next += 1;
    let mut attachments = Vec::with_capacity(message.attachments.len());
    for (attachment, opener) in message.attachments.iter().zip(&store.openers) {
        attachments.push(match attachment {
            AttachmentV2::File {
                name,
                mime,
                content_id,
                inline,
                size,
                ..
            } => UnicodePstAttachment {
                filename: name,
                mime_type: mime,
                content_id: content_id.as_deref(),
                data: &[],
                stream: opener.as_ref().map(|open| UnicodePstAttachmentStream {
                    len: *size,
                    open: open.as_ref(),
                }),
                hidden: *inline,
                attached_message: None,
            },
            AttachmentV2::Message { name, message, .. } => {
                let (message, attachments) = writer_parts(message, stores, recipients, next);
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
                }
            }
        });
    }
    let written = UnicodePstMessage {
        message_class: store.class,
        subject: &message.subject,
        // The manifest carries SMTP addresses only; sent-representing mirrors the sender.
        sender: message.sender.as_ref().map(|sender| UnicodePstSender {
            name: &sender.name,
            email: &sender.email,
            address_type: "SMTP",
        }),
        recipients: store_recipients,
        body: &message.body,
        html_body: message.html.as_deref().filter(|html| !html.is_empty()),
        message_id: message.message_id.as_deref(),
        delivery_time: store.delivery_time,
        creation_time: store.creation_time,
        last_modification_time: store.modification_time,
        search_key: Some(&store.search_key),
        is_read: message.is_read,
        importance: importance(message.importance), // the manifest checks reject values above 2
        client_submit_time: store.submit_time,
        categories: &store.categories,
        transport_headers: message.transport_headers.as_deref(),
        unsent: message.is_draft,
        named_properties: &store.named,
        properties: &store.given,
    };
    (written, attachments)
}

fn importance(value: u8) -> UnicodePstImportance {
    match value {
        0 => UnicodePstImportance::Low,
        1 => UnicodePstImportance::Normal,
        _ => UnicodePstImportance::High, // the manifest checks reject values above 2
    }
}

/// Latest supported time: 9999-12-31 23:59:59 UTC. Later FILETIMEs are valid on disk but readers built on .NET DateTime
/// and common clients cannot represent them, so such an item would be written and then not verifiable; it is refused
/// at the boundary instead.
const MAX_UNIX_SECONDS: i64 = 253_402_300_799;

fn filetime(field: &str, unix_seconds: i64) -> Result<i64, Failure> {
    unix_seconds
        .checked_add(FILETIME_EPOCH_OFFSET)
        .and_then(|seconds| seconds.checked_mul(10_000_000))
        .filter(|value| *value >= 0 && unix_seconds <= MAX_UNIX_SECONDS)
        .ok_or_else(|| {
            Failure::invalid(
                None,
                format!("{field} is outside the supported range 1601-01-01..9999-12-31"),
            )
        })
}

/// The decimal byte sizes a manifest v2 export's files may have (Purview eDiscovery's choices, 1 GB = 10^9 bytes). The
/// default is the largest, 10 GB.
pub(crate) const FILE_SIZE_CHOICES: &[(&str, u64)] = &[
    ("1GB", 1_000_000_000),
    ("2GB", 2_000_000_000),
    ("5GB", 5_000_000_000),
    ("10GB", 10_000_000_000),
];
pub(crate) const DEFAULT_FILE_SIZE: u64 = 10_000_000_000;

/// Where a conversion writes.
pub(crate) enum Target {
    /// One file, `--output`. For manifest v2 it has the file size limit; an export that does not fit fails by name.
    File(PathBuf),
    /// A new directory, `--output-dir` (manifest v2 only): `export-0001.pst`, `export-0002.pst` and so on, each with
    /// the whole declared folder tree, then items until the next commit would end the file past the limit.
    Directory(PathBuf),
}

/// The files of a conversion and the session writing the current one.
struct Sink {
    target: Target,
    /// The largest file a commit may leave (`None` for manifest v1: one file, no limit).
    limit: Option<u64>,
    /// Directory mode: the declared folders, in the records' order, created first in every file.
    tree: Vec<Vec<String>>,
    writer: Option<UnicodePstWriter>,
    /// The files created, in order (file mode: the output once it exists).
    files: Vec<PathBuf>,
    /// Items committed to the current file (directory mode).
    items_in_file: usize,
    /// The files above the limit, each holding one item larger than a file, with that item's key.
    /// The result names them (`file_size_limit.files_above`): the limit is not absolute, and that is said, not left to be
    /// inferred from file sizes.
    above_limit: Vec<(PathBuf, String)>,
}

fn limit_reached(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<UnicodePstFileLimitReached>())
}

fn output_too_large(limit: u64, item_key: Option<&str>, what: &str) -> Failure {
    Failure::new(
        "output-too-large",
        2,
        item_key,
        format!("{what} does not fit a file of {limit} bytes"),
    )
}

/// A new PST whose message store is named after its file, without the extension: classic Outlook shows an opened PST
/// under its store's PidTagDisplayName, which the template has as "Empty", the same in every export and in every part
/// of a split one. The file's name ("export-0002.pst", or the one `--output` gives) is the name; no manifest field.
fn create_store(path: &Path) -> io::Result<UnicodePstWriter> {
    let mut writer = UnicodePstWriter::create(path)?;
    // The file exists from here (the writer removes it itself if `create` fails), but the caller lists it only once
    // this returns. So a failure of the store name's own commit (ENOSPC) removes it here, or it would be left behind.
    if let Err(error) = writer.set_store_display_name(
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("export"),
    ) {
        drop(writer);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(writer)
}

impl Sink {
    /// Directory mode: the next file, with the whole declared tree in it.
    fn open_next(&mut self, dir: &Path) -> Result<(), Failure> {
        let path = dir.join(format!("export-{:04}.pst", self.files.len() + 1));
        let mut writer = create_store(&path)
            .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
        self.files.push(path);
        writer.set_max_file_size(self.limit);
        for folder in &self.tree {
            let names: Vec<&str> = folder.iter().map(String::as_str).collect();
            writer.ensure_folder_path(&names).map_err(|error| {
                match (limit_reached(&error), self.limit) {
                    (true, Some(limit)) => {
                        output_too_large(limit, None, "the declared folder tree alone")
                    }
                    _ => Failure::new("writer", 5, None, error.to_string()),
                }
            })?;
        }
        self.writer = Some(writer);
        self.items_in_file = 0;
        Ok(())
    }

    /// One commit: `members` of `rows` into `folder`. `payload` is what the members hold (bodies and attachments), used
    /// only to start a new file early, never to decide whether a commit fits: the writer decides that exactly.
    fn append(
        &mut self,
        folder: &[&str],
        members: &[usize],
        messages: &[UnicodePstBatchMessage<'_>],
        payload: &[u64],
        rows: &[Item],
        failure: &SourceFailure,
    ) -> Result<(), Failure> {
        let group: Vec<UnicodePstBatchMessage<'_>> =
            members.iter().map(|&index| messages[index]).collect();
        let fail = |error: io::Error| {
            failure
                .borrow_mut()
                .take()
                .unwrap_or_else(|| writer_failure(error, rows, members))
        };
        let dir = match &self.target {
            Target::File(path) => {
                let path = path.clone();
                let mut members = members;
                if self.files.is_empty() {
                    // The session that creates the file stays open for the whole conversion, so the file's state is
                    // read once. The first item is its own commit.
                    let first = members[0];
                    let mut session = create_store(&path).map_err(|error| {
                        failure
                            .borrow_mut()
                            .take()
                            .unwrap_or_else(|| writer_failure(error, rows, &[first]))
                    })?;
                    // The file is ours from here on: a failure of the first item removes it like any later failure.
                    self.files.push(path.clone());
                    session.set_max_file_size(self.limit);
                    session
                        .append_many_in_folder_with_attachments(folder, &[messages[first]])
                        .map_err(|error| match (limit_reached(&error), self.limit) {
                            (true, Some(limit)) => {
                                output_too_large(limit, Some(&rows[first].item_key), "the export")
                            }
                            _ => failure
                                .borrow_mut()
                                .take()
                                .unwrap_or_else(|| writer_failure(error, rows, &[first])),
                        })?;
                    self.writer = Some(session);
                    members = &members[1..];
                }
                if members.is_empty() {
                    return Ok(());
                }
                let group: Vec<UnicodePstBatchMessage<'_>> =
                    members.iter().map(|&index| messages[index]).collect();
                // One writer session for the whole conversion.
                let writer = self
                    .writer
                    .as_mut()
                    .expect("the file mode session exists once the file does");
                return writer
                    .append_many_in_folder_with_attachments(folder, &group)
                    .map_err(|error| match (limit_reached(&error), self.limit) {
                        (true, Some(limit)) => {
                            output_too_large(limit, Some(&rows[members[0]].item_key), "the export")
                        }
                        _ => failure
                            .borrow_mut()
                            .take()
                            .unwrap_or_else(|| writer_failure(error, rows, members)),
                    });
            }
            Target::Directory(dir) => dir.clone(),
        };
        let limit = self.limit.expect("directory mode has a limit");
        // Start the next file early when this commit would clearly not fit (it saves writing it twice).
        if self.writer.is_some() && self.items_in_file > 0 {
            let path = self.files.last().expect("a session writes the last file");
            let size = fs::metadata(path).map_err(Failure::from)?.len();
            if size.saturating_add(members.iter().map(|&index| payload[index]).sum::<u64>()) > limit
            {
                self.writer = None;
            }
        }
        loop {
            if self.writer.is_none() {
                self.open_next(&dir)?;
            }
            let writer = self.writer.as_mut().expect("opened above");
            match writer.append_many_in_folder_with_attachments(folder, &group) {
                Ok(()) => {
                    self.items_in_file += members.len();
                    return Ok(());
                }
                // The writer abandoned the commit and cut the file back to its last commit: it is complete as it is.
                Err(error) if limit_reached(&error) => {
                    self.writer = None;
                    if self.items_in_file > 0 {
                        continue;
                    }
                    if members.len() > 1 {
                        // Not even a file holding only the folder tree takes the whole commit: half by half.
                        let (first, second) = members.split_at(members.len() / 2);
                        self.open_next_again()?;
                        self.append(folder, first, messages, payload, rows, failure)?;
                        return self.append(folder, second, messages, payload, rows, failure);
                    }
                    // One item larger than a file gets a file of its own, above the limit.
                    let path = self.files.last().expect("the file just abandoned").clone();
                    let mut writer = UnicodePstWriter::open(&path)
                        .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
                    writer
                        .append_many_in_folder_with_attachments(folder, &group)
                        .map_err(fail)?;
                    writer
                        .close()
                        .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
                    self.items_in_file = members.len();
                    self.above_limit
                        .push((path, rows[members[0]].item_key.clone()));
                    return Ok(());
                }
                Err(error) => return Err(fail(error)),
            }
        }
    }

    /// After an abandoned commit in a file holding only the tree: the same file again, with a new session.
    fn open_next_again(&mut self) -> Result<(), Failure> {
        let path = self.files.last().expect("the file just abandoned").clone();
        let mut writer = UnicodePstWriter::open(&path)
            .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
        writer.set_max_file_size(self.limit);
        self.writer = Some(writer);
        self.items_in_file = 0;
        Ok(())
    }
}

/// What an item holds that the export writes: its bodies and its attachments' bytes, at every depth. Directory mode only
/// (to start a file early and to bound a batch); it decides nothing about correctness.
fn item_payload(item: &Item) -> u64 {
    fn files(message: &NestedMessage) -> u64 {
        message
            .attachments
            .iter()
            .map(|attachment| match attachment {
                AttachmentV2::File { size, .. } => *size,
                AttachmentV2::Message { message, .. } => files(message),
            })
            .sum()
    }
    item_bytes(item) as u64 + files(&item.message)
}

fn flush(rows: &[Item], sink: &mut Sink, unsupported: &mut Unsupported) -> Result<(), Failure> {
    if rows.is_empty() {
        return Ok(());
    }
    // Attachments are streamed by the writer from one-shot openers; nothing of them is read here except the empty
    // ones, which the writer never opens. Every message's store, item by item, depth first.
    let failure: SourceFailure = Rc::new(RefCell::new(None));
    let mut stores: Vec<MessageStore<'_>> = Vec::new();
    for item in rows {
        collect(
            &item.item_key,
            None,
            &item.message,
            &mut stores,
            &failure,
            unsupported,
        )?;
    }
    for item in rows {
        // Disclosure contract: a field counts as represented only when the writer writes the property with the
        // input's value. v1: an rfc822 file stands for an attached e-mail and is stored ATTACH_BY_VALUE, a disclosed
        // loss. v2: a file is a file, and an attached e-mail is written as a message inside its attachment.
        let rfc822 = item.message.attachments.iter().any(|a| matches!(a, AttachmentV2::File { mime, .. } if mime.eq_ignore_ascii_case("message/rfc822")));
        if !item.manifest_v2 && rfc822 {
            unsupported.note("embedded_message");
        }
    }
    let recipients = recipient_lists(&stores);
    let mut next_store = 0;
    let parts: Vec<(UnicodePstMessage<'_>, Vec<UnicodePstAttachment<'_>>)> = rows
        .iter()
        .map(|item| writer_parts(&item.message, &stores, &recipients, &mut next_store))
        .collect();
    let messages: Vec<UnicodePstBatchMessage<'_>> = parts
        .iter()
        .map(|(message, attachments)| UnicodePstBatchMessage {
            message: *message,
            attachments,
        })
        .collect();
    let payload: Vec<u64> = rows.iter().map(item_payload).collect();
    // A batch holds items of several folders; they are committed as one append per folder (folders in
    // order of first appearance, items in manifest order within a folder) instead of one commit per folder change.
    let mut groups: Vec<(&Vec<String>, Vec<usize>)> = Vec::new();
    for (index, item) in rows.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|(folder, _)| **folder == item.folder_path)
        {
            Some((_, members)) => members.push(index),
            None => groups.push((&item.folder_path, vec![index])),
        }
    }
    for (folder_path, members) in groups {
        check_stop()?;
        let folder: Vec<&str> = folder_path.iter().map(String::as_str).collect();
        sink.append(&folder, &members, &messages, &payload, rows, &failure)?;
    }
    Ok(())
}

/// Single-open manifest stream whose hash covers exactly the bytes handed to the parser.
struct ManifestReader {
    inner: BufReader<File>,
    hasher: Sha256,
    line: Vec<u8>,
}

impl ManifestReader {
    /// Next raw line including its terminator (hashed), returned as UTF-8; `None` at EOF.
    fn next_line(&mut self) -> Result<Option<String>, Failure> {
        self.line.clear();
        let n = self.inner.read_until(b'\n', &mut self.line)?;
        if n == 0 {
            return Ok(None);
        }
        self.hasher.update(&self.line);
        String::from_utf8(std::mem::take(&mut self.line))
            .map(Some)
            .map_err(|_| Failure::invalid(None, "manifest is not valid UTF-8"))
    }

    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

/// Bytes a batch holds in memory for an item: its body and HTML. Attachments are streamed by the writer and are
/// not held, so they do not count against --batch-bytes.
fn item_bytes(item: &Item) -> usize {
    fn given(given: &[Given]) -> usize {
        given.iter().map(|given| given.value.held()).sum()
    }
    fn held(message: &NestedMessage) -> usize {
        message.body.len()
            + message.html.as_ref().map_or(0, String::len)
            + message.transport_headers.as_ref().map_or(0, String::len)
            // The given properties are held too, the message's and its recipients', and the named ones.
            + given(&message.given)
            + message.named.iter().map(|named| named.name.len() + named.value.held()).sum::<usize>()
            + recipients_of(message).map(|(r, _)| given(&r.given)).sum::<usize>()
            + message.attachments.iter().map(|attachment| match attachment {
                AttachmentV2::Message { message, .. } => held(message),
                AttachmentV2::File { .. } => 0,
            }).sum::<usize>()
    }
    held(&item.message)
}

/// What this attempt created, for removal on failure: the output files, the output directory, the exclusion record.
#[derive(Default)]
struct Created {
    files: Vec<PathBuf>,
    directory: Option<PathBuf>,
    excluded: bool,
}

/// The record of the items left out (`--excluded`): one JSON line per item, in manifest order,
/// `{"item_key", "reason", "message_class", "at"}`. It carries the item's key, the reason code, the class that is not
/// converted (null for the nesting depth) and the attachment positions leading to the message concerned; no message
/// content. Its SHA-256 is computed from the bytes written.
struct ExclusionRecord {
    file: File,
    hasher: Sha256,
    items: usize,
}

impl ExclusionRecord {
    fn add(&mut self, item_key: &str, exclusion: &Exclusion) -> Result<(), Failure> {
        let mut line = serde_json::json!({ "item_key": item_key, "reason": exclusion.reason, "message_class": exclusion.message_class, "at": exclusion.at }).to_string();
        line.push('\n');
        self.hasher.update(line.as_bytes());
        self.file.write_all(line.as_bytes())?;
        self.items += 1;
        Ok(())
    }
}

thread_local! {
    /// The running conversion's cancel flag (`Options::cancel`), for the attachment readers the writer drives.
    static CANCEL: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { RefCell::new(None) };
}

/// Whether the running conversion was asked to stop.
fn stop_requested() -> bool {
    CANCEL.with(|cancel| {
        cancel
            .borrow()
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
    })
}

fn stopped() -> Failure {
    Failure::new("interrupted", 130, None, "the conversion was stopped")
}

fn check_stop() -> Result<(), Failure> {
    if stop_requested() {
        return Err(stopped());
    }
    Ok(())
}

/// Converts, and on failure removes what this attempt created: the output files, their directory and the exclusion
/// record, so no partial artifact is left behind.
pub(crate) fn run(options: &Options) -> Result<serde_json::Value, Failure> {
    let mut created = Created::default();
    CANCEL.with(|cancel| *cancel.borrow_mut() = options.cancel.clone());
    // A stop shows as whatever failure it caused (the writer's, an attachment's); it is reported as the stop.
    let result =
        convert(options, &mut created)
            .map_err(|failure| if stop_requested() { stopped() } else { failure });
    CANCEL.with(|cancel| *cancel.borrow_mut() = None);
    result.inspect_err(|_| {
        for path in &created.files {
            let _ = fs::remove_file(path);
        }
        if let Some(directory) = &created.directory {
            let _ = fs::remove_dir(directory);
        }
        if created.excluded {
            if let Some(path) = &options.excluded {
                let _ = fs::remove_file(path);
            }
        }
    })
}

fn convert(options: &Options, created: &mut Created) -> Result<serde_json::Value, Failure> {
    let output_path = match &options.output {
        Target::File(path) | Target::Directory(path) => path,
    };
    if output_path.exists() {
        return Err(Failure::new(
            "output-exists",
            4,
            None,
            "output path already exists; the caller owns a fresh temporary path",
        ));
    }
    if options.excluded.as_ref().is_some_and(|path| path.exists()) {
        return Err(Failure::new(
            "output-exists",
            4,
            None,
            "exclusion record path already exists; the caller owns a fresh temporary path",
        ));
    }
    // The manifest is opened once; every byte handed to the parser is hashed from that same
    // stream, so the echoed sha256 is the hash of exactly what was converted (line endings, trailing
    // bytes and all). The pathname is never reopened.
    let mut reader = ManifestReader {
        inner: BufReader::new(File::open(&options.manifest)?),
        hasher: Sha256::new(),
        line: Vec::new(),
    };
    let header_line = reader
        .next_line()?
        .ok_or_else(|| Failure::invalid(None, "manifest is empty"))?;
    let header_text = header_line.trim_end_matches(['\r', '\n']);
    // Only a header that parses as v2 takes the v2 path; every other header goes through v1's parse unchanged, so v1's
    // errors (duplicate keys included) stay what they were.
    let is_v2 = serde_json::from_str::<SchemaProbe>(header_text)
        .is_ok_and(|probe| probe.schema == MANIFEST_SCHEMA_V2);
    let (header, folder_count) = if is_v2 {
        let v2: HeaderV2 = serde_json::from_str(header_text)
            .map_err(|error| Failure::invalid(None, format!("header: {error}")))?;
        // A v2 export of folders only is a success; with neither folders nor items there is nothing to export.
        if v2.folder_count == 0 && v2.item_count > 0 {
            return Err(Failure::invalid(
                None,
                "header: folder_count is 0, but every item's folder must be declared",
            ));
        }
        if v2.folder_count == 0 {
            return Err(Failure::new(
                "empty-selection",
                2,
                None,
                "manifest declares zero folders and zero items; an empty export is not a success",
            ));
        }
        (
            Header {
                schema: v2.schema,
                export_id: v2.export_id,
                item_count: v2.item_count,
            },
            Some(v2.folder_count),
        )
    } else {
        let header: Header = serde_json::from_str(header_text)
            .map_err(|error| Failure::invalid(None, format!("header: {error}")))?;
        if header.schema != MANIFEST_SCHEMA {
            return Err(Failure::invalid(
                None,
                format!("unsupported manifest schema {:?}", header.schema),
            ));
        }
        if header.item_count == 0 {
            return Err(Failure::new(
                "empty-selection",
                2,
                None,
                "manifest declares zero items; an empty export is not a success",
            ));
        }
        (header, None)
    };
    // A file size limit and a directory of files belong to manifest v2: v1 is one file.
    if folder_count.is_none()
        && (options.file_size.is_some() || matches!(options.output, Target::Directory(_)))
    {
        return Err(Failure::new(
            "usage",
            2,
            None,
            "--output-dir and --max-file-size need a manifest v2",
        ));
    }
    // The exclusion record belongs to manifest v2: v1 says nothing a converter would leave out.
    let mut record = match &options.excluded {
        None => None,
        Some(_) if folder_count.is_none() => {
            return Err(Failure::new(
                "usage",
                2,
                None,
                "--excluded needs a manifest v2",
            ))
        }
        Some(path) => {
            let file = File::options().write(true).create_new(true).open(path).map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => Failure::new("output-exists", 4, None, "exclusion record path already exists; the caller owns a fresh temporary path"),
                _ => Failure::from(error),
            })?;
            created.excluded = true;
            Some(ExclusionRecord {
                file,
                hasher: Sha256::new(),
                items: 0,
            })
        }
    };
    let mut line_number = 1usize;
    // Manifest v2: the folder records come first, every path once; an item's folder must be one of them. `order` keeps
    // the records' order, in which the folders no item uses are created.
    let mut declared: Option<BTreeSet<Vec<String>>> = None;
    let mut order: Vec<Vec<String>> = Vec::new();
    if let Some(count) = folder_count {
        let mut folders = BTreeSet::new();
        while folders.len() < count {
            let line = reader.next_line()?.ok_or_else(|| {
                Failure::invalid(
                    None,
                    format!(
                        "manifest declares {count} folders but ends after {}",
                        folders.len()
                    ),
                )
            })?;
            line_number += 1;
            let line = line.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue;
            }
            let record: FolderRecord = serde_json::from_str(line).map_err(|error| {
                Failure::invalid(None, format!("line {line_number}: folder record: {error}"))
            })?;
            if let Some(problem) = folder_path_problem(&record.folder_path) {
                return Err(Failure::invalid(
                    None,
                    format!("line {line_number}: {problem}"),
                ));
            }
            if !folders.insert(record.folder_path.clone()) {
                return Err(Failure::invalid(
                    None,
                    format!("line {line_number}: a folder is declared twice"),
                ));
            }
            order.push(record.folder_path);
        }
        // The records are the export's folder tree: every folder's parent is declared too.
        if let Some(orphan) = folders
            .iter()
            .find(|path| path.len() > 1 && !folders.contains(&path[..path.len() - 1]))
        {
            return Err(Failure::invalid(
                None,
                format!(
                    "a folder record of depth {} is declared without its parent folder",
                    orphan.len()
                ),
            ));
        }
        declared = Some(folders);
    }
    let limit = declared
        .as_ref()
        .map(|_| options.file_size.unwrap_or(DEFAULT_FILE_SIZE));
    let target = match &options.output {
        Target::File(path) => Target::File(path.clone()),
        Target::Directory(path) => {
            fs::create_dir(path).map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => Failure::new(
                    "output-exists",
                    4,
                    None,
                    "output path already exists; the caller owns a fresh temporary path",
                ),
                _ => Failure::from(error),
            })?;
            created.directory = Some(path.clone());
            Target::Directory(path.clone())
        }
    };
    let directory = matches!(target, Target::Directory(_));
    let mut sink = Sink {
        target,
        limit,
        tree: if directory { order.clone() } else { Vec::new() },
        writer: None,
        files: Vec::new(),
        items_in_file: 0,
        above_limit: Vec::new(),
    };
    let outcome = convert_items(
        options,
        &mut reader,
        &mut sink,
        &mut record,
        &declared,
        &order,
        &header,
        line_number,
    );
    created.files = sink.files.clone();
    let (read, skipped, batches, unsupported) = outcome?;
    let excluded = match record {
        None => None,
        Some(mut record) => {
            record.file.flush()?;
            record.file.sync_all()?;
            let path = options.excluded.as_ref().expect("a record has a path");
            Some(
                serde_json::json!({ "path": path.to_string_lossy(), "items": record.items, "sha256": format!("{:x}", record.hasher.finalize()) }),
            )
        }
    };
    let manifest_sha256 = reader.finish();
    let outputs = sink.files.iter().map(|path| Ok(serde_json::json!({ "path": path.to_string_lossy(), "bytes": fs::metadata(path)?.len() }))).collect::<Result<Vec<_>, Failure>>()?;
    // The limit and every file above it, by name, with the one item it holds. A file above the limit
    // for any other reason (only a test limit below the empty store's size can make one) is refused, so the list is the
    // whole truth about the limit.
    let file_size_limit = match sink.limit {
        None => None,
        Some(limit) => {
            let mut above = Vec::new();
            for path in &sink.files {
                let bytes = fs::metadata(path)?.len();
                if bytes > limit {
                    let Some((_, item_key)) =
                        sink.above_limit.iter().find(|(above, _)| above == path)
                    else {
                        return Err(output_too_large(limit, None, "the empty store"));
                    };
                    above.push(serde_json::json!({ "path": path.to_string_lossy(), "bytes": bytes, "item_key": item_key }));
                }
            }
            Some(serde_json::json!({ "bytes": limit, "files_above": above }))
        }
    };
    let mut result = serde_json::json!({
        "protocol": RESULT_PROTOCOL,
        "engine": { "name": "pstsmith", "version": ADAPTER_VERSION, "writer": WRITER_VERSION },
        "manifest": { "schema": header.schema, "export_id": header.export_id, "item_count": header.item_count, "sha256": manifest_sha256 },
        "read": read,
        "written": read - skipped,
        "skipped": skipped,
        "batches": batches,
        "unsupported": unsupported.counts,
    });
    if directory {
        result["output"] = serde_json::Value::Null;
        result["outputs"] = serde_json::Value::Array(outputs);
    } else {
        result["output"] = outputs
            .into_iter()
            .next()
            .expect("file mode wrote its file");
    }
    if let Some(excluded) = excluded {
        result["excluded"] = excluded;
    }
    if let Some(file_size_limit) = file_size_limit {
        result["file_size_limit"] = file_size_limit;
    }
    Ok(result)
}

/// The items of the manifest, from the reader's position after the header and the folder records: read, validated,
/// left out or batched into the sink. Returns (read, skipped, batches, unsupported).
#[allow(clippy::too_many_arguments)]
fn convert_items(
    options: &Options,
    reader: &mut ManifestReader,
    sink: &mut Sink,
    record: &mut Option<ExclusionRecord>,
    declared: &Option<BTreeSet<Vec<String>>>,
    order: &[Vec<String>],
    header: &Header,
    mut line_number: usize,
) -> Result<(usize, usize, usize, Unsupported), Failure> {
    let mut rows: Vec<Item> = Vec::new();
    let mut rows_bytes = 0usize;
    let mut rows_payload = 0u64;
    let mut unsupported = Unsupported::default();
    let mut read = 0usize;
    let mut skipped = 0usize;
    let mut batches = 0usize;
    let directory = matches!(sink.target, Target::Directory(_));
    // Directory mode bounds a batch's payload too, so that a commit that does not fit the current file wastes at most
    // 1/64 of a file; file mode batches by --batch and --batch-bytes only.
    let payload_cap = if directory {
        sink.limit.map(|limit| limit / 64)
    } else {
        None
    };
    let mut used: BTreeSet<Vec<String>> = BTreeSet::new();
    while let Some(line) = reader.next_line()? {
        check_stop()?;
        line_number += 1;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        let item: Item = match &declared {
            None => {
                let item: ItemV1 = serde_json::from_str(line).map_err(|error| {
                    Failure::invalid(None, format!("line {line_number}: {error}"))
                })?;
                validate_item(&item)?;
                item_from_v1(item)
            }
            Some(_) => item_from_v2(serde_json::from_str::<ItemV2>(line).map_err(|error| {
                Failure::invalid(None, format!("line {line_number}: {error}"))
            })?)?,
        };
        if let Some(folders) = &declared {
            if !folders.contains(&item.folder_path) {
                return Err(Failure::invalid(
                    Some(&item.item_key),
                    "folder_path is not one of the manifest's folder records",
                ));
            }
        }
        read += 1;
        // An item the converter cannot write faithfully is left out by name, or, without a record to name it in, fails
        // the conversion.
        if item.manifest_v2 {
            if let Some(found) = exclusion(&item.message, 0, &mut Vec::new()) {
                let Some(record) = record.as_mut() else {
                    let detail = match &found.message_class {
                        Some(class) => format!("message class {class:?} at depth {}: only {} and reports about them are converted (calendar items, contacts, tasks and other classes are not converted)", found.at.len(), CONVERTED_CLASSES.join(", ")),
                        None => format!("attached e-mails nest deeper than {MAX_NESTING} levels"),
                    };
                    return Err(Failure::new(found.reason, 2, Some(&item.item_key), detail));
                };
                record.add(&item.item_key, &found)?;
                unsupported.note(found.disclosure());
                skipped += 1;
                continue;
            }
            for length in 1..=item.folder_path.len() {
                used.insert(item.folder_path[..length].to_vec());
            }
        }
        let size = item_bytes(&item);
        let payload = if payload_cap.is_some() {
            item_payload(&item)
        } else {
            0
        };
        if !rows.is_empty()
            && (rows.len() == options.batch_items
                || rows_bytes + size > options.batch_bytes
                || payload_cap.is_some_and(|cap| rows_payload + payload > cap))
        {
            flush(&rows, sink, &mut unsupported)?;
            batches += 1;
            rows.clear();
            rows_bytes = 0;
            rows_payload = 0;
        }
        rows_bytes += size;
        rows_payload += payload;
        rows.push(item);
    }
    check_stop()?;
    if !rows.is_empty() {
        flush(&rows, sink, &mut unsupported)?;
        batches += 1;
    }
    if read != header.item_count {
        return Err(Failure::invalid(
            None,
            format!(
                "manifest declares {} items but contains {read}",
                header.item_count
            ),
        ));
    }
    match sink.target {
        Target::Directory(ref dir) => {
            // Every file has the whole tree already; an export with no item written still has its first file.
            if sink.files.is_empty() {
                let dir = dir.clone();
                sink.open_next(&dir)?;
            }
        }
        Target::File(ref path) => {
            // A v2 export whose items were all left out, or that has no items, is still a store with its folders.
            if sink.writer.is_none() && declared.is_some() {
                let path = path.clone();
                let mut writer = create_store(&path)
                    .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
                sink.files.push(path);
                writer.set_max_file_size(sink.limit);
                sink.writer = Some(writer);
            }
            // Empty folders are preserved: a declared folder that no written item's path passes through is created
            // after the items, in the records' order (an existing one, such as the template's Deleted Items, writes
            // nothing).
            if let Some(writer) = sink.writer.as_mut() {
                for folder in order.iter().filter(|folder| !used.contains(*folder)) {
                    let names: Vec<&str> = folder.iter().map(String::as_str).collect();
                    writer.ensure_folder_path(&names).map_err(|error| {
                        match (limit_reached(&error), sink.limit) {
                            (true, Some(limit)) => output_too_large(limit, None, "the export"),
                            _ => Failure::new("writer", 5, None, error.to_string()),
                        }
                    })?;
                }
            }
        }
    }
    check_stop()?;
    if let Some(writer) = sink.writer.take() {
        writer
            .close()
            .map_err(|error| Failure::new("writer", 5, None, error.to_string()))?;
    }
    Ok((read, skipped, batches, unsupported))
}
