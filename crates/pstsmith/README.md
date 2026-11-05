# pstsmith

Read, create and append PST files in pure Rust, on any operating system, without Outlook or MAPI.

The PST format is documented in the [MS-PST](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/141923d5-15ab-4ef1-a524-6dce75aae546)
open specification. Type names follow that document, adjusted to Rust conventions, and doc comments link to the
sections they implement.

## Converting mail

`convert::mail::Converter` turns EML files, MBOX files (Google Takeout, Thunderbird, Apple Mail), Maildir directories
and directories of them into one PST file or size-limited parts; `convert::convert_manifest` does the same for a
JSONL manifest of messages ([format](https://github.com/baozdemir/pstsmith/blob/main/docs/MANIFEST.md)).

```rust,no_run
use pstsmith::convert::{mail::Converter, ManifestOptions, Output};

fn main() -> Result<(), pstsmith::convert::ConvertError> {
    let mut converter = Converter::create(Output::File("archive.pst".into()), &ManifestOptions::default())?;
    converter.add_path("mail/Inbox.mbox".as_ref())?;
    let report = converter.finish()?;
    println!("{} messages, {} warnings", report.written(), report.warnings().len());
    Ok(())
}
```

A failed conversion removes everything it created. The features `convert` and `mail` are on by default; without
them the crate is the reader and writer alone.

## Reading

`UnicodePstFile::open` and `AnsiPstFile::open` (with `open_store`) read Unicode (Outlook 2003 and later) and ANSI
(Outlook 97-2002) files: the store, folders, contents and hierarchy tables, messages, recipients, attachments and
named properties.

## Writing

`UnicodePstWriter` creates a Unicode PST or opens an existing one and appends messages to folder paths relative to
the IPM subtree (such as `&["Projects", "2026"]`), creating missing folders. Folder names are matched exactly, without
case folding. Messages carry recipients, plain text and HTML bodies, attachments (streamed, any size the format allows),
attached e-mails, categories and further tagged or named properties.

- Every commit is atomic: the header's switch to the new B-tree roots is the commit point. An error before it leaves
  the file at its last committed state and poisons the session; reopening continues from there.
- A new file starts as a copy of the bundled template store, which is permute-encoded as Outlook writes it. Appending
  works on unencrypted and permute-encoded Unicode files, including files Outlook created; cyclic encoding and ANSI
  files are refused.
- Blocks a commit replaces are removed from the B-trees and the allocation map, and later allocations reuse that space
  first-fit. The file is never compacted.

```rust,no_run
# use pstsmith::{UnicodePstBatchMessage, UnicodePstFile};
# fn example(batch: &[UnicodePstBatchMessage<'_>]) -> std::io::Result<()> {
UnicodePstFile::append_many_with_attachments("export.pst", batch)?;
# Ok(())
# }
```

See [`docs/UPSTREAM.md`](https://github.com/baozdemir/pstsmith/blob/main/docs/UPSTREAM.md) for how this crate relates to `microsoft/outlook-pst-rs` and
`dongs0104/outlook-pst-rs`.
