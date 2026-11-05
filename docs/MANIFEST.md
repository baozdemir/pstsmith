# The pstsmith manifest

A manifest is a JSONL file that lists mail items, with everything pstsmith needs to write them into a PST file: folders,
messages, recipients, attachments (as paths to local files with their size and SHA-256), attached e-mails and further
MAPI properties. It is the input for a producer that already holds mail as data, such as a mailbox export from
Microsoft Graph, rather than as EML or MBOX files. pstsmith answers with one `pst-export-result/1` JSON line.

## Overview

| Schema | Content |
|---|---|
| `pst-export-manifest/1` | A header line, then one line per item. Folders come from the items' paths; attachments are files. |
| `pst-export-manifest/2` | A header line, the folder records (the whole folder tree, empty folders included), then the items. Adds the message class, drafts, creation and modification times, transport headers, given properties (tagged and named), attached e-mails, items left out by name and size-limited output parts. |

Run it from the command line:

```
pstsmith --manifest ITEMS.jsonl (--output FILE.pst | --output-dir DIR)
         [--max-file-size 1GB|2GB|5GB|10GB] [--excluded PATH] [--batch N] [--batch-bytes N]
```

or from Rust, with `pstsmith::convert::convert_manifest(manifest: &Path, output: &Output, options: &ManifestOptions)
-> Result<Report, ConvertError>`:

- `Output::File(path)` or `Output::Directory(path)` (manifest v2 only). The path must not exist.
- `ManifestOptions { max_file_size, excluded, batch_items, batch_bytes }`; `ManifestOptions::default()` is no limit given
  (10 GB is used for v2), no exclusion record, 4,096 items and 64 MiB per batch. `FILE_SIZE_CHOICES` lists the four sizes.
- `Report::result_line()` is the result line; `written()`, `skipped()` and `outputs()` read it.
- `ConvertError { code, exit_code, item_key, detail }`, with `error_line()` for the stderr line.

The manifest is a versioned public format. The header's `schema` names the version; a change that would refuse a valid
manifest or write it differently gets a new schema number, and older schemas keep being read as before.

## File layout

- UTF-8, one JSON object per line, lines ending in LF (a CR before it is ignored). Not UTF-8 is `invalid-manifest`.
- The first line is the header. After it, a line that is empty or only white space is skipped.
- v2 requires every key of every object. A value that may be absent is `null`, never a missing key. Unknown keys are
  refused at every level, in v1 and v2.
- The manifest is read once, as a stream, in batches. Its SHA-256 over exactly the bytes read is echoed in the result.
- The JSON parser (serde_json) refuses a line nested deeper than 128 arrays and objects (`invalid-manifest`, "recursion
  limit exceeded"). Each level of attached e-mail takes three of them (`attachments`, the attachment, its `message`).

## Header

| Key | Type | v1 | v2 | Rules |
|---|---|---|---|---|
| `schema` | string | `pst-export-manifest/1` | `pst-export-manifest/2` | Any other schema is `invalid-manifest`. |
| `export_id` | string | yes | yes | The producer's identity of the export, echoed in the result. |
| `item_count` | integer >= 0 | yes | yes | Must equal the number of item lines (fewer or more is `invalid-manifest`). |
| `folder_count` | integer >= 0 | no | yes | The number of folder records that follow. |

- v1: `item_count` 0 is `empty-selection`.
- v2: `item_count` 0 with folders is a success, a store holding the declared folders and no message. `folder_count` 0
  with items is `invalid-manifest`; `folder_count` and `item_count` both 0 is `empty-selection`.

## Folder records (v2)

`folder_count` lines `{"folder_path": [string, ...]}` follow the header. Together they are the export's folder tree.

- A path is a non-empty list of non-empty names, each at most 1,790 UTF-16 code units (one heap allocation, MS-PST
  2.3.1). The same rule holds for an item's `folder_path`, in v1 too.
- Names are compared exactly, with no case folding and no trimming: `Inbox` and `inbox` are two folders.
- No path appears twice, and every folder's parent is declared too.
- Every declared folder is created, empty ones included. A new store already has Inbox and Deleted Items; declaring them
  uses them, and they stay empty when the export does not use them. No other folder is created under the IPM subtree.
- A manifest that ends before `folder_count` records is `invalid-manifest`.

## Item (v2)

| Key | Type | Null | Written as, and rules |
|---|---|---|---|
| `item_key` | string | no | The producer's own identity of the item, non-empty. Named in errors and the exclusion record; not written. |
| `search_key` | string | no | PidTagSearchKey, 32 hexadecimal digits (16 bytes). |
| `message_id` | string | yes | PidTagInternetMessageId, non-empty. Null writes none (a draft may have none yet). |
| `message_class` | string | no | PidTagMessageClass, written as given. See "Message classes". |
| `folder_path` | [string] | no | Must be one of the folder records. |
| `subject` | string | no | PidTagSubject, may be empty. MS-PST 2.5.3.1 calculates PidTagSubjectPrefix and PidTagNormalizedSubject. |
| `sender` | object | yes | `{"name": string, "email": string}`: the sender and the sent-representing identity, address type SMTP. An empty `name` is written as the address (MS-OXCMAIL 2.2.3.2). Null writes none of their six properties (0x0042, 0x0064, 0x0065, 0x0C1A, 0x0C1E, 0x0C1F). |
| `to`, `cc`, `bcc` | [recipient] | no | The recipient table, in this order. |
| `body` | string | no | The plain text body. |
| `html` | string | yes | The HTML body. Null writes none; an empty string is written as absent and disclosed (`empty_html_treated_as_absent`). |
| `sent_time` | integer | yes | PidTagClientSubmitTime. Null means not sent and writes none. |
| `received_time` | integer | yes | PidTagMessageDeliveryTime. Null writes none (a draft has not been received). |
| `created_time` | integer | no | PidTagCreationTime. |
| `last_modified_time` | integer | no | PidTagLastModificationTime, in the message and in its contents-table row. |
| `is_draft` | bool | no | mfUnsent in PidTagMessageFlags (MS-OXCMSG 2.2.1.6). A draft has `sent_time` null. |
| `is_read` | bool | no | The read state. |
| `importance` | integer | no | PidTagImportance: 0 low, 1 normal, 2 high. |
| `categories` | [string] | no | The categories (PidNameKeywords), written as given. Outlook and Exchange separate categories with commas and semicolons, and a Microsoft 365 import dropped a message whose category had a comma; leave both out of names meant for an import. |
| `transport_headers` | string | yes | PidTagTransportMessageHeaders, exactly as given; an empty string is written present and empty. Null writes none. |
| `properties` | [property] | no | Given properties. See "Given properties". |
| `attachments` | [attachment] | no | In display order. |

Times are whole Unix seconds (UTC) from -11644473600 (1601-01-01) to 253402300799 (9999-12-31 23:59:59); a time
outside that range is `invalid-manifest`, also inside an attached e-mail and in an item that is left out.

**Recipient:** `{"name": string, "email": string, "properties": [property]}`. An empty `name` is written as the address
(MS-OXCMAIL 2.2.3.2). `properties` are tagged properties written in the recipient's row (see "Given properties").

### Attachment of kind `file`

`{"kind": "file", "path", "name", "mime", "content_id", "inline", "size", "sha256"}`

| Key | Type | Rules |
|---|---|---|
| `path` | string | A local file, opened as given (a relative path is relative to the working directory). It is opened once and read once while the writer writes it; the bytes written are the bytes checked. A file that cannot be read is `source-unreadable`; another size or digest is `source-mismatch` (exit 3). |
| `name` | string | Non-empty. PidTagAttachLongFilename, PidTagAttachFilename and PidTagDisplayName. PidTagAttachExtension is "." and the part after the last period, Unicode separators as spaces and leading and trailing spaces and periods removed; none when that part is empty (MS-OXCMAIL 2.2.3.4.1.1, steps 1 to 3). |
| `mime` | string | PidTagAttachMimeTag. |
| `content_id` | string or null | PidTagAttachContentId, stored without one leading `<` and one trailing `>` (`<part1@example.invalid>` and `part1@example.invalid` store the same value). One that is empty without them (`""`, `"<>"`) is `invalid-manifest`. |
| `inline` | bool | True: PidTagAttachmentHidden, and PidTagAttachFlags ATT_MHTML_REF (0x4), MS-OXCMAIL's inline mark (2.1.3.4.1.2). An inline file needs a `content_id`. A listed file (false) keeps its Content-ID without the mark, which could hide it (2.2.3.4.1.4.1). |
| `size` | integer | The file's length in bytes. |
| `sha256` | string | The file's SHA-256, 64 hexadecimal digits, either case. |

A file is stored by value (PidTagAttachMethod 1) whatever its MIME type: an `.eml` given as a file stays a file.

### Attachment of kind `message`

`{"kind": "message", "name": string, "inline": false, "message": nested}` is an attached e-mail.

- `name` is non-empty and written as PidTagDisplayName. `inline` must be `false`: an attached e-mail has no Content-ID.
- `nested` has the item's keys except `item_key` and `folder_path`, with the same rules, at any depth.
- It is written as PidTagAttachMethod 5 with PidTagAttachDataObject, whose node holds a complete message (MS-PST
  2.4.6.2.2); it has no file name, MIME type or Content-ID.
- Its attachments may be attached e-mails again, up to 32 levels below the item; see "Items left out by name".

## Given properties

`properties` carry the properties the manifest has no field for, such as a report's (MS-OXOMSG 2.2.2, MS-OXCMAIL
2.2.3.7) or a meeting message's (MS-OXOCAL). pstsmith writes them as given and does not judge what they mean.

**Form.** Tagged: `{"tag": "0x" + four hexadecimal digits, "type", "value"}`. Named, for messages only: `{"set": GUID,
"lid": integer, "type", "value"}` or `{"set": GUID, "name": string, "type", "value"}`. The GUID is
`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in hexadecimal, either case, without braces; a LID is 0 to 4294967295. The keys of
the other form are left out, never null.

| `type` | `value` | MAPI type |
|---|---|---|
| `int32` | JSON integer, -2^31 to 2^31 - 1 | PtypInteger32 |
| `bool` | `true` or `false` | PtypBoolean |
| `time` | JSON integer, FILETIME (100 ns since 1601-01-01 UTC), 0 to 2650467743999999999 (9999-12-31 23:59:59.9999999), written exactly | PtypTime |
| `string` | JSON string; empty is written present and empty | PtypString |
| `binary` | an even number of hexadecimal digits; empty is written present and empty | PtypBinary |

**Rules** (a broken one is `invalid-manifest`, at any depth):

- A tag is 0x0001 to 0x7FFF. A named property is never given by an ID, which is the store's: give its set and LID or name.
- A tag is not one pstsmith writes from its own fields or one MS-PST calculates. For a message: 0x0017, 0x001A, 0x0037,
  0x0039, 0x003D, 0x0042, 0x0064, 0x0065, 0x0070, 0x007D, 0x0C1A, 0x0C1E, 0x0C1F, 0x0E02, 0x0E03, 0x0E04, 0x0E06, 0x0E07,
  0x0E08, 0x0E09, 0x0E12, 0x0E13, 0x0E17, 0x0E1B, 0x0E1D, 0x0FFF, 0x1000, 0x1013, 0x1035, 0x3001, 0x3007, 0x3008,
  0x300B, 0x3013, 0x3FDE, 0x67F2 and 0x67F3 (`MESSAGE_OWNED_IDS`). For a recipient: 0x0C15, 0x0E0F, 0x0FF9, 0x0FFE,
  0x0FFF, 0x3001, 0x3002, 0x3003, 0x300B, 0x3900, 0x39FE, 0x39FF, 0x3A40, 0x67F2 and 0x67F3 (`RECIPIENT_OWNED_IDS`).
- No tag twice in one list. Within one message, a recipient tag has one type for every recipient.
- These tags must have their MS-OXPROPS type (report properties, the tagged properties MS-OXOCAL 2.2.1 to 2.2.12 names,
  and the template's contents-table columns); any other tag takes the type given:

| Type | Tags |
|---|---|
| `int32` | 0x0036, 0x0062, 0x0C04, 0x0C05, 0x0C20, 0x0E38, 0x1080, 0x1097, 0x5FFD, 0x5FFF, 0x65C6, 0x6869, 0x7FFD |
| `bool` | 0x0057, 0x0058, 0x0063, 0x0C17, 0x5FE1, 0x686D, 0x686E, 0x686F, 0x7D01, 0x7FFE |
| `time` | 0x0010, 0x002A, 0x0032, 0x004E, 0x0055, 0x0060, 0x0061, 0x5FE3, 0x5FE4, 0x5FFB, 0x7FF9, 0x7FFB, 0x7FFC |
| `string` | 0x003A, 0x0049, 0x004D, 0x005A, 0x005D, 0x0066 to 0x0069, 0x0072 to 0x0074, 0x0080, 0x0081, 0x0C1B, 0x0C21, 0x1001, 0x1039, 0x1046, 0x3613, 0x36E5, 0x4029 to 0x402B, 0x6820 |
| `binary` | 0x0025, 0x0031, 0x0045, 0x0046, 0x0053, 0x0054, 0x005B, 0x005C, 0x005E, 0x005F, 0x0071, 0x0E30, 0x0E34, 0x0E3C, 0x0E3D, 0x3A12, 0x686A |

  0x0E33 is PtypInteger64, which the manifest cannot give, so it is always refused.
- A named property is not on a recipient; not in PS_MAPI {00020328-0000-0000-C000-000000000046}, whose LIDs are tags
  (give the tag); has a non-empty name; in PS_INTERNET_HEADERS {00020386-0000-0000-C000-000000000046} has a header field
  name (RFC 5322: printable ASCII, no colon); is not PS_PUBLIC_STRINGS "Keywords", which is `categories`; and is not given
  twice in one message.
- **How names match** (MS-OXCPRPT, RopGetPropertyIdsFromNames): a LID matches a LID in the same set; a string name matches
  exactly, except in PS_INTERNET_HEADERS, where it is lower-cased first and written lower-cased. So `Content-Type` and
  `content-type` are one name there, and two names elsewhere.
- The named properties MS-OXOCAL 2.2.1 to 2.2.9 names must have their MS-OXPROPS type:

| Set | `int32` | `bool` | `time` | `string` | `binary` |
|---|---|---|---|---|---|
| PSETID_Appointment {00062002-0000-0000-C000-000000000046} | 0x8201, 0x8203 to 0x8205, 0x8207, 0x8213, 0x8214, 0x8217, 0x8218, 0x8224, 0x8231, 0x8241, 0x8256, 0x8259 | 0x8206, 0x820A, 0x8215, 0x8223, 0x8229, 0x822B, 0x823A, 0x8240, 0x8244, 0x8246, 0x8257, 0x825A | 0x8202, 0x820D, 0x820E, 0x8220, 0x8228, 0x8235, 0x8236, 0x8250, 0x8251 | 0x8208, 0x8209, 0x8230, 0x8232, 0x8234, 0x8238, 0x823B, 0x823C, 0x8242, 0x8243, 0x8247 to 0x8249 | 0x8216, 0x8233, 0x8237, 0x825D to 0x8261 |
| PSETID_Common {00062008-0000-0000-C000-000000000046} | 0x8510 | 0x8045 | 0x8516, 0x8517 | 0x8536 to 0x8538 | |
| PSETID_Meeting {6ED8DA90-450B-101B-98DA-00AA003F1305} | 0x000C, 0x001C, 0x0026 | 0x0004, 0x0005, 0x000A | 0x0001, 0x001A, 0x0029, 0x002A | 0x0002, 0x0024, 0x0028 | 0x0003, 0x0023 |
| PSETID_CalendarAssistant {11000E07-B51B-40D6-AF21-CAA85EDAB1D0} | 0x0015, 0x85CD | 0x85CC | | | |
| PS_PUBLIC_STRINGS {00020329-0000-0000-C000-000000000046} | | name `DoNotForward` | | | |

**Where they are written.** A message's, in its property context and in its folder's contents-table row where that
table has a column with the same tag and type. A recipient's, in the recipient table: one column per tag that any
recipient of the message gives, in tag order (MS-PST 2.3.4.1); a recipient that does not give it has no cell there. A
cell value larger than one heap allocation (3,580 bytes) is a subnode of its table (MS-PST 2.3.4.4). A named property is
written under the ID the store's name-to-ID map gives its name (MS-PST 2.4.7); a new name is added to the map. The map
ends at 0xFFFE; a name past it fails the conversion (`writer`).

## Message classes

- A class follows MS-OXCMSG 2.2.1.3: 1 to 255 characters, all ASCII 0x20 to 0x7F, not ending with a period. Another class,
  at any depth, is `invalid-manifest`. Classes match case-insensitively (`ipm.note` is IPM.Note) and are written as given.
- **Converted:**
  - IPM.Note.
  - Signed and encrypted mail as MS-OXOSMIME 2.1.3 stores it: IPM.Note.SMIME.MultipartSigned (clear-signed), IPM.Note.SMIME
    (opaque-signed or encrypted) and IPM.Note.Receipt.SMIME (a secure receipt). The protected content is the one
    `smime.p7m` file attachment: for a clear-signed message the whole `multipart/signed` entity with its Content-Type
    header field, for the others the CMS object. It is written byte for byte, so a signature stays verifiable; the
    structure is not checked. Such a class without a `smime.p7m` file attachment (name matched case-insensitively) is
    written as IPM.Note, with everything else as given, and disclosed (`smime_protected_content_missing`): classic
    Outlook for Windows cannot open it as S/MIME ("An error occurred in the underlying security system").
  - Meeting messages (MS-OXOCAL 2.2.6 to 2.2.9): IPM.Schedule.Meeting.Request, IPM.Schedule.Meeting.Resp.Pos, .Resp.Tent
    and .Resp.Neg, IPM.Schedule.Meeting.Canceled and IPM.Schedule.Meeting.Notification.Forward. A class with a further
    suffix, such as a custom form's `IPM.Schedule.Meeting.Request.X`, is not converted.
  - Reports about any class above: `REPORT.` + the class + a suffix, one of `.DR`, `.Expanded.DR`, `.Relayed.DR`,
    `.Delayed.DR` and `.NDR` (delivery status, MS-OXCMAIL 2.2.3.7.1.1) or `.IPNRN` and `.IPNNRN` (read and not-read
    notifications, 2.2.3.7.2), matched case-insensitively. So `REPORT.IPM.Note.NDR` and `REPORT.IPM.Note.SMIME.IPNRN` are
    converted. A report's own properties are given properties.
- **Every other class** (calendar items, contacts, tasks, notes and so on) is not written as mail, which would lose what
  makes it that class. The item is left out by name (below), also when the class is that of an attached e-mail.

**Delivery status reports.** A recipient's status is PidTagNonDeliveryReportStatusCode (0x0C20) for `.NDR` and
PidTagDeliverTime (0x0010) for the other delivery status suffixes. A report that lacks it for a recipient, or has no
recipient, is written and disclosed (`report_recipient_status`). A read notification has no recipient status.

**Meeting messages.** Their properties are given properties: the named ones (PidLid*) by set and LID, the tagged ones by
tag, and attendees' as recipient properties (required, optional and resource attendees as `to`, `cc` and `bcc`, MS-OXOCAL
2.2.4.10.7). Three losses are disclosed per message:

- `meeting_core_properties`: no PidLidGlobalObjectId (PSETID_Meeting LID 0x0003), PidLidAppointmentStartWhole or
  PidLidAppointmentEndWhole (PSETID_Appointment LIDs 0x820D and 0x820E), the message's identity and time.
- `meeting_exception_attachments`: PidLidAppointmentRecur (PSETID_Appointment 0x8216) has a modified instance. Its
  Exception Attachment objects (MS-OXOCAL 2.2.10) are not written, as the manifest has no attachment-level properties.
  The recurrence BLOB itself is written as given; deleted instances lose nothing.
- `meeting_recurrence_unreadable`: PidLidAppointmentRecur is not a whole AppointmentRecurrencePattern (MS-OXOCAL
  2.2.1.44.5: its RecurrencePattern, 2.2.1.44.1, then ReaderVersion2 0x3006, WriterVersion2, the time offsets,
  ExceptionCount equal to ModifiedInstanceCount, each ExceptionInfo, an empty ReservedBlock1, each ExtendedException and
  an empty ReservedBlock2); or there is no BLOB while PidLidIsRecurring (PSETID_Meeting 0x0005) is true, unless the
  message is an exception (PidLidIsException, PSETID_Meeting 0x000A, true and PidLidRecurring, PSETID_Appointment
  0x8223, not true). The BLOB, PidLidRecurring and PidLidIsRecurring are then left out and the message is otherwise
  written in full. Classic Outlook for Windows does not open such a message with them ("Cannot locate recurrence
  information for this appointment"); it opens it without them, an exception without a BLOB, and one with only
  PidLidRecurring true. Flags given false lose nothing. Bytes after ReservedBlock2 are written as given.

## Disclosures

The result's `unsupported` maps each name below to the number of times it occurred; a name that did not occur is
absent, and `{}` means nothing was disclosed. Each counts once per message: an item's own or an attached e-mail at any
depth. Items left out count once per item.

| Name | Meaning |
|---|---|
| `empty_html_treated_as_absent` | An empty `html` string, written as no HTML body (v1 and v2). |
| `embedded_message` | v1 only: an item with a `message/rfc822` file, stored as a file and not as an attached e-mail. |
| `smime_protected_content_missing` | An S/MIME class without its `smime.p7m` file, written as IPM.Note. |
| `report_recipient_status` | A delivery status report without every recipient's status. |
| `meeting_core_properties`, `meeting_exception_attachments`, `meeting_recurrence_unreadable` | See "Meeting messages". |
| `excluded_item_class`, `excluded_nesting_depth` | An item left out by name, for its class or its depth. |

## Items left out by name (v2)

- An item is walked depth first (a message before the e-mails attached to it, attachments in order). The first message
  that is more than 32 levels below the item, or has a class that is not converted, decides the reason:
  `nesting-too-deep` or `unsupported-item-class`. The whole item is left out, not only the attachment: writing the
  item without it would change the mail silently.
- An item left out must still be valid: every rule above holds for it, at every depth.
- **With `--excluded PATH`** the rest of the export converts. pstsmith creates PATH (it must not exist) and writes one
  line per item left out, in manifest order: `{"item_key": string, "reason": string, "message_class": string or null,
  "at": [integer]}`. `message_class` is the class not converted, null for the depth; `at` is the attachment positions
  leading to the message concerned, empty for the item's own message and never empty for the depth. The line carries
  no message content. The item's folder is still created, as a declared folder. The result gets `skipped`, `written`
  and `excluded`, and `unsupported` counts each reason.
- **Without `--excluded`** such an item fails the conversion: `unsupported-item-class` or `nesting-too-deep`, exit 2,
  and everything created is removed.

## Manifest v1

A v1 item has `item_key`, `search_key`, `message_id`, `folder_path`, `subject`, `sender`, `to`, `cc`, `bcc`, `body`,
`html`, `sent_time`, `received_time`, `is_read`, `importance`, `categories` and `attachments`, with v2's rules except:

- `message_id` is a non-empty string, `sender` an object and `received_time` an integer: none of them null.
- Recipients are `{"name", "email"}`. Attachments are files without `kind`: `{"path", "name", "mime", "content_id",
  "inline", "size", "sha256"}`. `html`, `sent_time` and `content_id` may be null or left out.
- The class is IPM.Note, the item is not a draft, and `created_time` and `last_modified_time` are `received_time`.
- There are no folder records: each item's `folder_path` is created when first used.
- `--output-dir`, `--max-file-size` and `--excluded` are `usage` errors: v1 writes one file without a size limit.

## Output

- **`--output FILE`** writes one file. For v2 it has the size limit; an export that does not fit is `output-too-large`
  (exit 2), and nothing is left behind.
- **`--output-dir DIR`** (v2 only) creates DIR, which must not exist, and writes `export-0001.pst`, `export-0002.pst`
  and so on, in order:
  - every file starts with the whole declared folder tree, then items until the next commit would not fit; that commit
    goes to the next file. Such a file is therefore not byte-identical to the one-file output of the same manifest;
  - a batch's payload (bodies and attachment bytes) is bounded to 1/64 of the limit, and a commit that does not fit
    even a file holding only the tree is split in halves;
  - **the limit is not absolute**: a single item larger than the limit gets a file of its own, above it, and the result
    names that file and item in `file_size_limit.files_above`. A message whose size does not fit the 32-bit
    PidTagMessageSize or PidTagAttachSize is `item-too-large`, so this happens only with the 1 GB and 2 GB choices.
- **`--max-file-size`** is `1GB`, `2GB`, `5GB` or `10GB` (decimal, 1 GB = 10^9 bytes), the default `10GB`. The writer
  enforces it exactly: a commit that would end a file past it is abandoned and the file cut back to its last commit.
- **Batches.** `--batch N` (default 4,096 items) and `--batch-bytes N` (default 64 MiB of bodies, HTML, transport
  headers and given property values held in memory; attachments are streamed and do not count) bound each batch. Both
  must be at least 1.
- Each file's message store is named after the file without its extension (`export-0002`, or the `--output` name),
  which classic Outlook for Windows shows for an opened PST.
- A build with the feature `test-file-sizes` accepts `--max-file-bytes N` for tests; release builds refuse it.

## Errors

| Code | Exit | When |
|---|---|---|
| `invalid-manifest` | 2 | Any manifest rule above is broken: a parse error, an unknown or missing key, a bad header or folder record, an undeclared folder, a wrong item count, a bad search key or class, an empty `message_id`, a draft with a `sent_time`, a time out of range, an inline attached e-mail, a given property that breaks its rules. |
| `empty-selection` | 2 | Nothing to export (see "Header"). |
| `unsupported-item-class`, `nesting-too-deep` | 2 | Without `--excluded`: an item that would be left out. |
| `item-too-large` | 2 | A message too large for the PST format's 32-bit message and attachment sizes. |
| `output-too-large` | 2 | v2: the export does not fit the limit in one file, or the declared folder tree alone does not fit one file. |
| `usage` | 2 | Bad arguments: no `--manifest`, none or both of `--output` and `--output-dir`, an unknown flag, a size not among the four, a batch limit of 0, or a v2 option with a manifest v1. |
| `source-unreadable` | 3 | An attachment file cannot be opened or read. |
| `source-mismatch` | 3 | An attachment file's size or SHA-256 differs from the manifest's. |
| `output-exists` | 4 | The output path or the exclusion record path exists. |
| `writer` | 5 | The writer or the file system failed, for example out of space. |
| `io` | 5 | Another I/O failure, such as a manifest that cannot be opened. |

- The error is one JSON line on stderr: `{"error": code, "item_key": string or null, "detail": string}`. It carries
  no subjects, bodies, recipient names or file names. For `writer`, `item_key` is the item whose message was being
  written when it failed, and null when the failure belongs to the file or the commit; it locates the failure and does
  not say the item is at fault.
- Because the manifest is read in batches, an error in a later item can come after earlier items were written. On any
  failure pstsmith removes what it created: the output files, their directory and the exclusion record.

## Result line

On success pstsmith prints one JSON line on stdout and exits 0.

| Field | Content |
|---|---|
| `protocol` | `"pst-export-result/1"`. |
| `engine` | `{"name": "pstsmith", "version": the crate version, "writer": "pstsmith " + version}`. |
| `manifest` | `{"schema", "export_id", "item_count", "sha256"}`: the header's values, as parsed, and the SHA-256 of the manifest bytes read. |
| `read` | Item lines read, items left out included. |
| `written` | Items written: `read - skipped`. |
| `skipped` | Items left out by name (0 for v1). |
| `batches` | The number of batches written. |
| `unsupported` | The disclosures (see "Disclosures"). |
| `output` | File mode: `{"path", "bytes"}`. Directory mode: `null`. |
| `outputs` | Directory mode only: `[{"path", "bytes"}, ...]`, in order. |
| `excluded` | With `--excluded` only: `{"path", "items", "sha256"}`, the number of lines and the SHA-256 of the record's bytes. |
| `file_size_limit` | v2 only: `{"bytes": the limit, "files_above": [{"path", "bytes", "item_key"}, ...]}`, every file above the limit and the one item it holds. |

## Example

A manifest v2 with one folder and one item with one file attachment. The file `attachments/hello.txt` holds the 12 bytes
`Hello, Bob.` and a line feed.

```jsonl
{"schema":"pst-export-manifest/2","export_id":"example-export","item_count":1,"folder_count":1}
{"folder_path":["Inbox"]}
{"item_key":"item-1","search_key":"0123456789abcdef0123456789abcdef","message_id":"<hello-1@example.invalid>","message_class":"IPM.Note","folder_path":["Inbox"],"subject":"Hello","sender":{"name":"Alice Example","email":"alice@example.invalid"},"to":[{"name":"Bob Example","email":"bob@example.invalid","properties":[]}],"cc":[],"bcc":[],"body":"Hello, Bob. The file is attached.\r\n","html":null,"sent_time":1767225600,"received_time":1767225605,"created_time":1767225605,"last_modified_time":1767225605,"is_draft":false,"is_read":true,"importance":1,"categories":[],"transport_headers":null,"properties":[{"tag":"0x0071","type":"binary","value":"01dc7a1b2c3d4e5f60718293a4b5c6d7e8f901234567"},{"set":"00020386-0000-0000-C000-000000000046","name":"X-Example","type":"string","value":"yes"}],"attachments":[{"kind":"file","path":"attachments/hello.txt","name":"hello.txt","mime":"text/plain","content_id":null,"inline":false,"size":12,"sha256":"6eb606435fff3047558e114c057c700d4bb027f198832853032b833ed8765a56"}]}
```

`pstsmith --manifest example.jsonl --output example.pst` writes `example.pst` and reports `read` 1, `written` 1,
`skipped` 0, `unsupported` `{}` and `file_size_limit` `{"bytes": 10000000000, "files_above": []}`. The named property is
stored as `x-example` in PS_INTERNET_HEADERS.

## Producing a manifest from Microsoft Graph

pstsmith writes what it is given. This is what the producer of the manifest needs to give when its source is Exchange
Online through Microsoft Graph.

- **Message class.** Graph does not return PR_MESSAGE_CLASS directly. Read the extended property `String 0x001A` (or map
  the item's `@odata.type`) and give the stored class unchanged; pstsmith decides what it converts.
- **Drafts.** `is_draft` is Graph's `isDraft`. A draft's sender, Message-ID and received time are given as Graph gives
  them: null when Graph has none.
- **Times.** `created_time`, `last_modified_time`, `received_time` and `sent_time` are Graph's `createdDateTime`,
  `lastModifiedDateTime`, `receivedDateTime` (null for a draft) and `sentDateTime`, as Unix seconds.
- **Folder tree.** From the mailbox's folder hierarchy, empty folders included.
- **Transport headers.** The message's own header block, byte for byte. Graph's `internetMessageHeaders` is a list of
  name and value pairs, and rebuilding the block from it can change folding and order. The extended property
  `String 0x007D` (PR_TRANSPORT_MESSAGE_HEADERS, through `singleValueExtendedProperties`) gives the stored text.
- **Attachments.** A Graph `itemAttachment` of type `message` becomes `kind: "message"`, a `fileAttachment` becomes
  `kind: "file"`. The `name` of an attached e-mail is Graph's `itemAttachment.name`, unchanged: the stored display name,
  which Outlook sets to the attached message's subject with no extension; adding ".msg" shows as "<subject>.msg.msg" in
  classic Outlook for Windows. An S/MIME message's `smime.p7m` is given as Graph's `fileAttachment` gives it, with its
  stored MIME type.
- **Depth.** Attached e-mails need to be expanded to at most 33 levels below the item: the 33rd already leaves the item
  out, and nothing deeper is needed to say so.
- **Extended properties.** Graph gives MAPI properties as `singleValueExtendedProperties` with an `id` such as
  `Integer {00062008-0000-0000-C000-000000000046} Id 0x8501` (a LID), `String {00020386-0000-0000-C000-000000000046}
  Name content-type` (a string name) or `String 0x007D` (a tag). Give the set, the LID or name, and the type as Graph
  gives them. Graph cannot list them all: "You can't use the REST API to get all the extended properties of a specific
  instance in one call" (Microsoft Graph, "Outlook extended properties overview"). The producer asks for each property
  it carries, by id, so which properties a PST carries is the producer's list, decided per item class.
- **Internet headers as named properties.** MS-OXCMAIL 2.2.3.2.29: a MIME reader stores each header of a received
  message as a PS_INTERNET_HEADERS named property (the header's name, its decoded value), except headers mapped to other
  properties and the ones that section lists; Exchange may skip one on its named-property quota (MS-OXCMAIL product
  behavior note 141). Graph cannot list which ones a message has. Take the header names from `internetMessageHeaders`,
  ask for `String {00020386-0000-0000-C000-000000000046} Name <name>` for each name not on 2.2.3.2.29's list, plus
  `content-type` for every S/MIME message (MS-OXOSMIME 2.1.3.2: an opaque-signed or encrypted message SHOULD have it),
  and give what Graph returns as named properties in PS_INTERNET_HEADERS. pstsmith stores the names lower-cased.
- **Meeting messages.** Graph gives a meeting message as an `eventMessage`. Ask for each property MS-OXOCAL 2.2.1 to
  2.2.12 names (the typed lists above) by its extended-property id and give what Graph returns, unchanged; attendees'
  recipient properties likewise. The manifest has no multi-valued type, so the multi-valued ones, on organizers'
  calendar objects, are not carried. The subject is Graph's, with its prefix ("Accepted: ..."); PidTagSubjectPrefix and
  PidTagNormalizedSubject are never given (MS-PST 2.5.3.1 calculates them).
- **Conversation index.** Give Graph's `conversationIndex` (message and eventMessage, Edm.Binary, "the position of the
  message within the conversation") as the given property 0x0071 `binary` (PidTagConversationIndex) whenever Graph has
  one. pstsmith derives none: a derived index would claim a thread position and conversation GUID the source never
  gave, change between runs, and merge or split threads. Outlook for Mac groups messages that carry none across
  unrelated items and files.
- **Reports.** Graph's object model has none of a report's recipient-level properties.
  - Read the report's MIME (`GET .../messages/{id}/$value`) and derive them as MS-OXCMAIL 2.2.3.7's MIME reader would
    store them: per failed or delivered recipient the status, the diagnostic and reason codes, the remote MTA, the
    supplementary text and the report time; for the message the reporting MTA, or a read notification's properties.
    `tests/oracle/report_reference.py` is a reference statement of these rules in the manifest's property form.
  - Read the report properties Graph gives as extended properties too. Where both give a tag, Graph's value is the
    stored one and wins; the derived one only fills a gap. The derived values are those MS-OXCMAIL's reader would
    store, not necessarily those Exchange stored.
  - A delivery status notification names a recipient by address only, so give `name` equal to `email` (an empty `name`
    writes the same file).
  - A recipient whose status neither source gives is left without it; pstsmith discloses the loss
    (`report_recipient_status`) instead of inventing it.
