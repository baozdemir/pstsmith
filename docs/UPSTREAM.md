# Relationship to upstream

## Lineage

1. [microsoft/outlook-pst-rs](https://github.com/microsoft/outlook-pst-rs): an MS-PST reader, read-only by design, MIT.
2. [dongs0104/outlook-pst-rs](https://github.com/dongs0104/outlook-pst-rs): adds creation and incremental append of
   Unicode PST files, crate `outlook-pst-rw` 1.2.14, forked here at commit `b0d0fafc79df97f7ae529f2a5c2e2285c1a015af`.
3. pstsmith: this repository, a hard fork of that commit, MIT.

The git history of this repository keeps every upstream commit. pstsmith is not affiliated with or endorsed by
Microsoft.

Section numbers refer to MS-PST v20250218 and to MS-OXCMSG, MS-OXOMSG, MS-OXCFOLD, MS-OXPROPS and MS-OXCMAIL
v20250520. What classic Outlook for Windows or SCANPST does is an observation of those programs, not the specification.

## What pstsmith changes

### Commit model and crash safety

- The commit model is upstream's: new blocks and B-tree pages go only into space the committed allocation map marks
  free or past the committed end of the file, and the header's root switch is the commit point (MS-PST 2.2.2.6,
  2.2.2.5).
- Space a commit frees (superseded blocks, replaced B-tree pages) returns to the allocation map after the commit point.
  The map page of a new range past the committed end is written before it, which is safe because the committed state
  ends before that range; the range holding the committed end and every committed map page are written after it.
- Any error poisons the writer session: later calls fail until the file is opened again, and an error before the root
  switch leaves the file at its last committed state.
- When a session opens, every AMap page is compared with the bits the B-trees give; a map that differs or is marked
  invalid (fAMapValid, MS-PST 2.6.1.3.7) is rebuilt before anything else is written.

### The writer session and per-commit cost

- `UnicodePstWriter` reads the file's committed state once, when it opens; `UnicodePstWriter::create` makes a new file
  from the template, with its Inbox, as an open session. Every older entry point (`append*`, `append_in_folder*`,
  `create*`, `set_store_display_name`) opens a session, commits once and closes it.
  `commit_many_in_folder_with_attachments` commits like `append_many_in_folder_with_attachments` without opening the
  file again afterwards.
- A commit's changes are a delta, so its work follows the change, not the size of the file:
  - each B-tree stays on disk behind a cache of at most 1,024 decoded pages (see "B-trees and data trees");
  - the allocation map keeps no record per block: a running free-byte total, at most 1,024 free runs below the end of
    the file, and at most 64 map pages read on demand;
  - block reference counts (BBTENTRY cRef, MS-PST 2.2.2.7.7.3) come from the delta; a full recount remains the
    fallback when the stored counts do not add up;
  - per folder appended to, only the roots of the contents table's heap and row-matrix data trees, the row count, the
    last row ID and the verified unread count are kept. An append reads heap page 0, the pages holding the TCINFO and
    the row-index header, the row index's right spine, the last heap page and the last row block (MS-PST 2.3.4);
  - folder paths, each parent's children by name and the PidNameKeywords ID are cached per session;
  - each message's property context is built once; PidTagMessageSize, an inline PtypInteger32, is then set in the
    encoded page through a BTH walk over the page maps (MS-PST 2.3.1.5, 2.3.2, 2.3.3.3).
- The search-management queue (MS-PST 2.4.8.4.1) is appended in place: only its last leaf, last XBLOCK and XXBLOCK
  are rewritten. A queue of another shape is rewritten in full.
- `UnicodePstAttachment::stream` takes a `UnicodePstAttachmentStream { len, open }` (then `data` must be empty). The
  writer calls `open` once, reads exactly `len` bytes into the same blocks as for a slice and requires the end of the
  stream; a short stream, extra bytes or a read error fail the append before its commit. Blocks go out in groups of
  128, encoded in place in reused buffers, so neither the value nor an allocation per block is held.
- Still proportional to the data: the first append to a folder in a session reads its whole contents table (to
  recount unread rows); each parent's hierarchy table is read once per session; a table whose layout the append view
  does not take (rows in the heap, another data-tree shape) is rebuilt in full.

### Allocation map, free space and file end

- Superseded blocks are removed from the B-trees and the allocation map, and later allocations reuse those free runs
  first-fit. The file is never compacted.
- The file grows in whole AMap ranges of 253,952 bytes (MS-PST 2.2.2.7.2; 2.6.1.1.2: it "MUST grow at integer
  multiples of the number of bytes mapped by an AMap"). A commit extends the file to the end of its last range and
  offers that tail as a free run to the next commit; cbAMapFree equals the free runs below the end. A file that ended
  at its last allocation, with the rest of the range counted free, had its tail counted twice once classic Outlook
  grew it from the range's end, and SCANPST then reported cbAMapFree and AMap csFree mismatches.
- Opening a session keeps the free tail of the last range even behind 1,024 or more free runs, so reopening a file
  does not grow it.
- FMap pages (MS-PST 2.2.2.7.5) are 0xFF, written once with their range; a file whose FMap pages are not all 0xFF
  has its allocation map rebuilt when opened. 0xFF is not the exact value 2.2.2.7.5 describes, but it is what classic
  Outlook writes and it never falls below the real free run: Outlook never updates FMaps (MS-PST 2.6.1), so exact
  values fall behind once it changes the file, and SCANPST reports only values below the real run. Readers SHOULD NOT
  use FMaps.
- PMap (2.2.2.7.3) and FPMap (2.2.2.7.6) pages are 0xFF too, written once with their range. The density list
  (2.2.2.7.4) and the header's free size are updated in memory and written when they change.

### B-trees and data trees

- A commit loads only the paths of the keys it changes and writes copy-on-write replacements of those leaves and their
  ancestors: no rebalancing, a page left empty is removed, a parent's key is its child's first key, at most 20 BBT or
  intermediate entries and 15 NBT leaf entries per page (cEntMax, MS-PST 2.2.2.7.7.1). An unchanged tree is untouched.
- New blocks have keys above every committed key, so their entries are buffered (4,096) and appended to a right edge
  started from a copy of the committed right-most path; every page the edge leaves behind is full, and the old path is
  freed after the commit point. Lookups return I/O errors, and every caller propagates them.
- Data trees have one shape: every leaf but the last full, 1,021 BIDs per XBLOCK ((8,176 − 8) / 8), every XBLOCK under
  an XXBLOCK but the last full (MS-PST 2.2.2.8.3.2.1, 2.2.2.8.3.2.2). An edit writes the new leaves, one XBLOCK per
  touched group and the XXBLOCK; other XBLOCKs keep their IDs. A tree whose shape is not proven is rebuilt.

### Heaps, contents and hierarchy tables

- Every subnode the LTP layer creates (row matrices, external property values, attachment data) has NID_TYPE_LTP
  (0x1F), as in Outlook-written files (MS-PST 2.2.2.1, 2.4.5). With NID_TYPE_INTERNAL the first row matrix, 0x21, is
  also NID_MESSAGE_STORE (2.4.1), and libpff permute-decodes it and misreads every row.
- A property context's BTH header is at HID 0x20, a table's row-index header at 0x20 and its TCINFO at 0x40, as in
  Outlook-written heaps. MS-PST allows any hidUserRoot, but pst-extractor (java-libpst family) reads only those.
- Each heap block stores the fill level of its free space after the page map, block 0 in the low nibble of the first
  byte (HNHDR, HNBITMAPHDR; MS-PST 2.3.1.2, 2.3.1.4). A writer that later allocates in the heap, such as Outlook,
  relies on these levels.
- A non-final heap page has no padding allocation: its block is still 8,176 bytes, the page map follows the last
  allocation, zero bytes run to the block's end and count as free. SCANPST reports padding allocations as errors and its
  repair produces this layout. MS-PST 2.3.1.5 speaks of "0 to 63 bytes of padding"; SCANPST's repair leaves up to
  1,576, and a contents table written over many small commits up to about 2,616, a layout not yet shown to SCANPST
  or Outlook.
- A heap allocation is at most 3,580 bytes (MS-PST 2.3.1). A larger value is a subnode (2.3.3.3); a larger table cell
  is a NID_TYPE_LTP subnode of its table, named by the cell's HNID (2.3.4.4), in contents, recipient and attachment
  tables.
- A contents-table or attachment-table cell in a subnode shares the block of the same property in the row's object
  when length and a 64-bit SipHash (fixed keys) agree; the block's reference count counts both. Classic Outlook stores
  such cells so, and SCANPST reports "row doesn't match sub-object" for a copy. Recipient cells keep their own subnodes.
- A hierarchy-table rebuild that keeps the heap frees what it writes anew (old TCINFO, old row-index nodes, an in-heap
  row matrix, old values of re-encoded rows), so the table holds no unreferenced allocations and does not grow with
  the square of its rows.
- Hierarchy rows are found through the row-index BTH (MS-PST 2.3.4.3) and patched in place, after checking the row ID:
  the parent's row mirroring a folder's counters and the grandparent's row gaining PidTagSubfolders. Template rows keep
  their replication cells (0x0E30, 0x0E33, 0x0E34). A new folder's row is appended along the changed path.
- A rebuilt table writes a present, empty cell (HNID 0, existence bit set) again as present and empty.
- PidTagContentUnreadCount is kept in the folder and in its parent's hierarchy row (MS-PST 2.4.4.3; MS-OXCFOLD
  2.2.2.2.1.2). At the first append to a folder in a session the unread rows are recounted from the PidTagMessageFlags
  cells; a usable stored count (Integer32, not negative) must agree or the append is refused before anything is
  written, an unusable one is replaced, and a count that cannot be established is an error.
- A parent hierarchy table whose 0x3602 or 0x3603 column is not PtypInteger32 (MS-PST 2.4.4.4.1) is refused before
  anything is written.

### Message properties written

- `search_key` sets PidTagSearchKey (0x300B; MS-PST 2.4.5.1.1, MS-OXPROPS 2.999) and its contents cell, since two
  items can share one Message-ID; `None` derives it from the Message-ID bytes, as upstream does, and a message with
  neither is refused.
- `message_class` is written as given (PidTagMessageClass and the 0x001A cell): 1 to 255 characters, ASCII 0x20 to
  0x7F, not ending with a period (MS-OXCMSG 2.2.1.3).
- PidTagMessageFlags takes MSGFLAG_READ from `is_read` and mfUnsent (a draft) from `unsent` (MS-OXCMSG 2.2.1.6);
  PidTagImportance comes from `importance` (2.2.1.11). PidTagPriority is not written: MS-OXCMAIL 2.2.3.2.5 maps the
  Importance and X-Priority headers only to PidTagImportance.
- `sender` writes the sender and sent-representing properties (0x0042, 0x0064, 0x0065, 0x0C1A, 0x0C1E, 0x0C1F;
  MS-OXOMSG 2.2.1.48, 2.2.1.51, 2.2.1.54, 2.2.1.55, 2.2.1.57), sent-representing mirroring the sender. An empty
  sender or recipient name is written as the address, as MS-OXCMAIL 2.2.3.2's MIME reader stores it.
- Absent optional values write no property and no contents cell: `sender`, `message_id` (0x1035, MS-OXOMSG
  2.2.1.12), `delivery_time` (0x0E06, 2.2.3.9), `client_submit_time` (0x0039, 2.2.3.11), `transport_headers`
  (PidTagTransportMessageHeaders 0x007D, 2.2.1.61, written exactly as given). `creation_time` and
  `last_modification_time` are required (MS-OXCMSG 2.2.2.3, 2.2.2.2).
- A message property context has no PidTagLtpRowId or PidTagLtpRowVer (0x67F2, 0x67F3); the contents row keeps both.
  MS-PST does not settle it (2.3.4.4.1 makes them the first columns of every row, 2.4.5.1.1 does not list them for a
  message, 2.4.4.5.1 marks them copied into the row); Outlook-written property contexts carry neither.
- Single-valued strings have no terminator, as in Outlook-written stores. MS-OXCDATA 2.11.1's terminated PtypString
  is the protocols' wire form, and MS-PST does not say whether a property context stores the terminator. Kept with a
  NUL: one-off entry ID strings (MS-OXCDATA 2.2.5.1) and the recipient search key `SMTP:ADDRESS`, a binary value.
- A present, empty value is HNID 0 (MS-PST 2.3.3.2).
- PidTagMessageSize is every byte the message's node and subnodes hold at every depth, row-matrix blocks at 8,176
  bytes but the last, the attachment table only when there are attachments. Classic Outlook stores this value, and
  where SCANPST flagged a differing value its repair set it to the bytes held. A shared attachment-table cell is
  counted both as the attachment's value and as the cell; Outlook's rule there is not known.
- `UnicodePstMessage::properties` and `UnicodePstRecipient::properties` (`UnicodePstProperty { id, value }`, Integer32,
  Boolean, Time, Unicode, Binary or MultipleUnicode) are written as given after the writer's own. An ID is in
  0x0001..=0x7FFF, not in `MESSAGE_OWNED_IDS` or `RECIPIENT_OWNED_IDS`, once per list, with one type across a
  message's recipients; no time is negative. A contents column of the same ID and type takes the value; another type
  leaves no cell (MS-PST 2.3.4.4.1).

### Recipients

- Every ID a message's recipients give becomes a recipient-table column. 4-byte cells follow the fixed ones from offset
  52, 8-byte cells follow with no padding, then the 1-byte cells (fixed 0x0E0F and 0x3A40 included) and the existence
  bitmap (MS-PST 2.3.4.4.1); descriptors are sorted by tag (2.3.4.1). Rows per block follow the row width.
- No row byte lies outside a column: for a gap SCANPST reports "COLDESCs do not account for all the space in the row"
  and its repair deletes every recipient. Outlook's own tables never pad. Without given properties the fixed columns
  are unchanged.

### Attachments and attached e-mails

- `hidden` sets PidTagAttachmentHidden (0x7FFE; MS-OXCMSG 2.2.2.24). ATT_MHTML_REF (PidTagAttachFlags 0x4, MS-OXCMAIL
  2.1.3.4.1.2) is set only on a hidden attachment; on a listed one with a Content-ID it "can result in the attachment
  being hidden from recipients" (2.2.3.4.1.4.1). The Content-ID is written either way.
- A file attachment has PidTagDisplayName (0x3001), the name as given, and PidTagAttachExtension (0x3703), "." and the
  part after the last period with leading and trailing spaces (Zs, Zl, Zp included) and periods removed, none when
  that part is empty or there is no period (MS-OXCMAIL 2.2.3.4.1.1 step 1). Outlook for Mac's PST import names
  attachments from PidTagDisplayName and shows "ATTnnnn" without it. The attachment table keeps the template's six
  columns (MS-PST 2.4.6.1.2).
- A by-value PidTagAttachSize is 4 (itself) plus the data's length plus each stored value's size (Boolean 1,
  Integer32 4, a string its UTF-16 bytes, binary its length; another type is refused). Outlook's own attachments
  follow this rule, and SCANPST reports "missing or invalid PR_ATTACH_SIZE" for a value one byte short.
- An attachment's property context is built like a message's: values over 3,580 bytes are NID_TYPE_LTP subnodes of
  the attachment from 0x80, the file data is 0x200 plus its position, and the heap takes as many pages as it needs.
- `UnicodePstAttachment::attached_message` holds a `UnicodePstAttachedMessage` by value; such an attachment has a name
  and no data, stream or Content-ID. The message is a NID_TYPE_NORMAL_MESSAGE subnode (0x200 plus the position) in
  the attachment's subnode tree, with its own recipients and attachments (MS-PST 2.4.6.2.2), nesting at most 32 deep.
- Its attachment holds PidTagAttachSize, PidTagDisplayName, PidTagAttachDataObject (PtypObject `{Nid, ulSize}`,
  MS-PST 2.3.3.5, MS-OXCMSG 2.2.2.8), PidTagAttachMethod 5 (2.2.2.9), PidTagRenderingPosition −1 and
  PidTagAttachmentHidden false; the table row has method 5 and no PidTagAttachFilename. As in Outlook-written files,
  ulSize is the attached message's PidTagMessageSize and PidTagAttachSize is the other values plus ulSize.

### Named properties and categories

- `categories` are one PtypMultipleString, in input order, under the ID the store's map gives PS_PUBLIC_STRINGS
  "Keywords" (PidNameKeywords, MS-OXCMSG 2.2.1.17; 0x8013 in the template). No "Keywords" entry is added: a store
  without one, or a target contents table with a column of that ID, refuses categories before anything is written.
- `named_properties` (`UnicodePstNamedProperty { set, name, value }`, a LID or a string name, the set in stored byte
  order, MS-OXCDATA 2.5.1) refuse PS_MAPI, an empty name, a PS_INTERNET_HEADERS name that is no header field name,
  PS_PUBLIC_STRINGS "Keywords", a name twice and a negative time. A LID matches within its set; a string exactly,
  lower-cased first in PS_INTERNET_HEADERS, both for given names and for names already in the map.
- The name-to-ID map (MS-PST 2.4.7) is read once per session and extended in the layout of Outlook-made stores: the
  next wPropIdx (NPID 0x8000 + wPropIdx, refused past 0xFFFE); wGuid 2 for PS_PUBLIC_STRINGS, else the set's GUID
  stream index plus 3, appending a new set; string names 4-byte aligned, length first, zero padded; records in bucket
  0x1000 + (dwPropertyID ^ ((wGuid << 1) | N)) % bucket count (MS-PST 2.4.7.5), N being 1 for a string, whose
  dwPropertyID is the MS-PST 5.3 CRC of its UTF-16 name; only non-empty buckets stored. Node 0x61 is rewritten in
  the same commit; the session keeps its copy only after the commit succeeds.
- Named properties follow the tagged ones in the property context and fill a contents column of their ID and type.

### Folders and names

- Folder names are matched exactly (no case folding): on paths, in `ensure_folder_path` and when looking for "Inbox".
  When two children share a name, the first in row order is found.
- `UnicodePstWriter::ensure_folder_path(&[&str])` makes a path exist under the IPM subtree, each missing folder in its
  own commit; an existing path writes nothing (MS-PST 2.4.4; PidTagSubfolders, MS-OXCFOLD 2.2.2.2.1.12).
- An append without a folder files into the folder named exactly "Inbox", else into the folder the receive-folder
  table names only when it is the IPM subtree's root or below it; otherwise it fails with `NotFound` before anything
  is written, because classic Outlook shows no mail outside the IPM subtree.
- A folder's property context is one heap page, so its display name is limited to 1,790 UTF-16 code units (one heap
  allocation, MS-PST 2.3.1).

### Block encoding on append

- Append supports unencrypted and permute-encoded Unicode PSTs and refuses cyclic encoding (`Unsupported`).
- A session encodes the blocks it writes as the file's header says: only the data field of data blocks (MS-PST
  2.2.2.8.3.1.1; the permute cipher, 5.1), never internal blocks; CRCs cover the stored bytes. A file written
  unencrypted stays unencrypted.
- A new file keeps the template's permute encoding (bCryptMethod 1), classic Outlook's default; some third-party
  readers guess at encryption in files marked unencrypted.

### Limits and refusals

- A message whose size, or an attachment's, does not fit a signed 32-bit integer is refused with kind `FileTooLarge`
  carrying `UnicodePstMessageTooLarge { position, size }`, before its streams are opened or any block is written:
  PidTagMessageSize and PidTagAttachSize are PtypInteger32 (MS-PST 2.4.5.1.1, 2.4.6.1.1, 2.4.6.2.1) and lcbTotal
  is 32 bits (2.2.2.8.3.2.1).
- `UnicodePstWriter::set_max_file_size(Option<u64>)` bounds the file every commit leaves (default none). A commit past
  the limit is abandoned before its map pages, length and header; the error (`FileTooLarge`,
  `UnicodePstFileLimitReached { size, limit }`) poisons the session, and the file is cut back to its last commit and
  synced; its free space may hold the abandoned bytes. Transient disk use is not bounded: a streamed attachment larger
  than the room left is written whole, then cut back.
- Other failures while preparing or writing a message carry `UnicodePstAppendFailed { position, detail }` with the
  cause's kind. The position is approximate (runs of up to 8 MiB are buffered, so an error can surface up to three
  messages later; the final flush and named-property resolution carry none); the commit fails whole.
- The input checks (`InvalidInput`) named in this document run before anything is written; any failure before the
  root switch leaves the file at its last commit.
- Not written: afStorage attachments (method 6). A file an earlier writer version wrote keeps its heap padding
  allocations when this version extends it; do not append across writer versions.

### Bugs fixed in upstream behaviour

- `PropertyType::try_from` lacked PtypObject (0x000D; MS-OXCDATA 2.11.1).
- The BTH leaf reader silently stopped at the first record it could not parse, so a property context with a PtypObject
  lost every later property. Every record is now read, and one that cannot be read is an error.
- `Attachment::read` for method 5 looked for the attached message in the parent message's subnodes, not the
  attachment's (MS-PST 2.4.6.2.2), and re-took the file lock it held. It now reads from the attachment's subnode tree
  after releasing the lock, and `UnicodeAttachment::attached_message()` returns the concrete type. The method-6
  (afStorage) read path keeps upstream's lookup.
- `NamedPropertyMapProperties::hash_entry` dropped a string name's wGuid, so lookups searched the wrong bucket; it now
  keeps it (MS-PST 2.4.7.5).
- A message without a subnode tree (bidSub 0) is read with no recipient or attachment table instead of failing:
  MS-PST 2.4.5.3 requires a recipient table, but Outlook writes such messages.
- `HeapNodePageMap::new` keeps the stored cFree and refuses only more freed items than items; another writer's
  zero-length allocation can be a referenced empty value.
- A variable-size or multi-valued value with HNID 0 is read as the empty value of its type, in a property context and a
  table cell (upstream: PtypNull, or `InvalidHeapIndex(0)`).
- `HeapFillLevel::pack_fill_levels` packed the first block into the high nibble; it now matches `unpack_fill_levels`.

## The template file

`crates/pstsmith/examples/Empty.pst` is an empty Unicode message store embedded with `include_bytes!`; the crate does
not build without it.

| Fact | Value |
|---|---|
| Size | 525,312 bytes |
| SHA-256 | `caa1df0d55d285b881b5ba2c8b35b02d4418d34683cbbb0a8fbd719968aa7b3e` |
| Header | magic `!BDN`, `wVer` 23 (Unicode), `wVerClient` 19, `bCryptMethod` 0x01 (permute) |
| Source | the file at dongs0104/outlook-pst-rs commit `b0d0fafc79df97f7ae529f2a5c2e2285c1a015af`, unchanged |

- Every new file starts as a byte copy of the template, kept as it is (permute-encoded); `create` then adds the Inbox.
  The template has no Inbox, its receive-folder table names the root folder 0x122, outside the IPM subtree, and its
  allocation map marks space no B-tree references, so the first session on a new file rebuilds the map.
- Its origin is not documented upstream. Upstream's test `appends_to_an_outlook_created_unicode_pst` calls it
  "Outlook's fixture" and asserts permute encoding; that claim has not been reproduced independently.
- It is distributed inside the MIT-licensed upstream repository. It is a data file, not program code, and its
  copyright status is unclear.
- Generating the template from code is planned. Until then this section fixes the exact bytes in use.

## Upstream changes not taken

pstsmith removes these upstream behaviours; each is described in its area above.

- Case-insensitive folder matching: ASCII-only folding merged distinct folders such as `Inbox` and `inbox` and left
  non-ASCII variants unmerged. Upstream's `creates_and_appends_messages_in_nested_folders` expects exact matching now.
- A folderless append into a receive folder outside the IPM subtree (the template's root folder, where classic Outlook
  shows no mail). Upstream's `appends_to_an_outlook_created_unicode_pst` now checks the refusal.
- Decoding the template and marking new files unencrypted, and refusing encoded files on append.
- A NUL terminator on single-valued strings, and PidTagLtpRowId and PidTagLtpRowVer in message property contexts.
- PidTagMessageSize as the bytes built less six (a rule upstream gives no source for), plus an attachment table not
  written for messages without attachments.
- Zero-filled padding allocations on non-final heap pages, every fill level stored as empty, and per-page FMap values.
- ATT_MHTML_REF on every attachment with a Content-ID, and NID_TYPE_INTERNAL for LTP subnodes.
- Fixed values in place of input: MSGFLAG_READ forced on, the class always `IPM.Note`, and the delivery time written
  as creation and modification time.
