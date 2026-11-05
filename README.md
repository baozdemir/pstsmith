<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/wordmark-dark.svg">
    <img alt="pstsmith" src="docs/assets/wordmark-light.svg" width="360">
  </picture>
</p>

<p align="center"><b>Read, create and append PST files in pure Rust, on any OS, without Outlook.</b></p>

<p align="center">
  <a href="https://crates.io/crates/pstsmith"><img alt="crates.io" src="https://img.shields.io/crates/v/pstsmith.svg"></a>
  <a href="https://docs.rs/pstsmith"><img alt="docs.rs" src="https://img.shields.io/docsrs/pstsmith"></a>
  <a href="https://github.com/baozdemir/pstsmith/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/baozdemir/pstsmith/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="MSRV 1.85" src="https://img.shields.io/badge/MSRV-1.85-orange.svg">
</p>

pstsmith turns mail into Outlook data files (`.pst`): EML files, MBOX files (Google Takeout, Thunderbird, Apple Mail),
Maildir directories, or a JSONL manifest of messages. It is a Rust library, a command-line tool and an npm package,
and it needs nothing else installed: no Outlook, no MAPI, no Windows.

> pstsmith is a hard fork of [`dongs0104/outlook-pst-rs`](https://github.com/dongs0104/outlook-pst-rs), itself a fork of
> [`microsoft/outlook-pst-rs`](https://github.com/microsoft/outlook-pst-rs). It is not affiliated with or endorsed by
> Microsoft. See [docs/UPSTREAM.md](docs/UPSTREAM.md).

```console
$ pstsmith convert "Takeout/Mail/All mail Including Spam and Trash.mbox" -o gmail.pst
pstsmith: wrote 7 messages to gmail.pst
```

## Highlights

- **Checked by Microsoft's own tools.** SCANPST finds only minor inconsistencies, with no `!!` or `??` line; classic
  Outlook opens the files; a Microsoft 365 (Purview) import accepted every test file with 0 bad items.
  [How it was verified](docs/VERIFICATION.md).
- **Read back by two independent readers,** libpff and XstReader, field by field.
- **Crash-safe appends.** Every commit is atomic: a crash or a full disk leaves the file at its last committed state.
- **Bounded memory.** The writer streams attachments from disk: 128,000 messages in one folder took 17 MB. Mail
  input is read one message at a time, so memory follows the largest message, not the size of the input.
- **Size-limited parts** (1, 2, 5 or 10 GB) for imports with per-file limits.
- **Same input, same bytes,** on Linux, macOS and Windows.
- **Nothing dropped.** Every MIME part becomes a body or an attachment; what cannot be shown as mail (an unreadable
  message, an attached e-mail nested too deep) is kept as an `.eml` attachment and reported.

## Install

```sh
cargo install pstsmith-cli          # or: cargo binstall pstsmith-cli
brew install baozdemir/tap/pstsmith
npm install pstsmith                # TypeScript/JavaScript API with prebuilt binaries
```

Prebuilt binaries for Linux (x86-64 and arm64, glibc and static musl), macOS (arm64 and x86-64) and Windows (x86-64)
are on the [releases page](https://github.com/baozdemir/pstsmith/releases). Each archive carries a build provenance
attestation: `gh attestation verify <file> --repo baozdemir/pstsmith`.

## Quick start

```sh
pstsmith convert mail/Inbox.mbox mail/Sent.mbox -o archive.pst      # one file
pstsmith convert ~/Maildir --output-dir parts --max-file-size 10GB   # parts: export-0001.pst, ...
pstsmith convert saved/ "Local Folders" -o mixed.pst --json           # a JSON result line on stdout
```

A directory is walked in name order: `.eml` files become a folder per directory, MBOX files and Thunderbird
`Name.sbd` / Apple Mail `Name.mbox` folders become folders, Maildirs become folders. Gmail labels become folders and
categories, each message written once. The decoded messages are staged in a private temporary directory
(`--temp-dir`), which needs about as much free space as the input.

**Rust**

```rust,no_run
use pstsmith::convert::{mail::Converter, ManifestOptions, Output};

fn main() -> Result<(), pstsmith::convert::ConvertError> {
    let mut converter = Converter::create(Output::File("archive.pst".into()), &ManifestOptions::default())?;
    converter.add_path("mail/Inbox.mbox".as_ref())?;
    converter.add_eml(&["Imported"], b"From: a@example.invalid\r\nSubject: Hello\r\n\r\nHi\r\n")?;
    let report = converter.finish()?;
    println!("{} messages", report.written());
    Ok(())
}
```

The writer API (`UnicodePstWriter`) appends messages, recipients, attachments and properties to new or existing files,
and the reader API reads Unicode and ANSI files; see [docs.rs](https://docs.rs/pstsmith).

**TypeScript**

```ts
import { convertMail } from "pstsmith";

const report = await convertMail({ inputs: ["mail/Inbox.mbox"], output: { file: "archive.pst" } });
console.log(report.written);
```

The conversion runs in a child process, so a failure never takes down your process, and an `AbortSignal` stops it.

## What it supports

| | |
|---|---|
| Read | Unicode and ANSI PST files |
| Create | Unicode PST files (Outlook 2003 and later) |
| Append | Unicode files, unencrypted or permute-encoded (as Outlook writes them), including files Outlook created |
| Input | EML, MBOX (mboxo/mboxrd, LF or CRLF), Maildir and Maildir++, Thunderbird and Apple Mail folders, JSONL manifest ([format](docs/MANIFEST.md)) |
| Mail | plain and HTML bodies, inline images, attachments, attached e-mails, S/MIME signed and encrypted mail, delivery and read reports, categories, read and draft state, importance, the original headers |

**When not to use pstsmith:** to write ANSI (Outlook 97-2002) files, to read OST files, to convert contacts, calendars
or tasks as such, or to recover a damaged PST. For reading and recovery, [libpff](https://github.com/libyal/libpff)
and readpst serve well; [PST Builder](https://github.com/ml6719/PST-Builder) writes contacts and calendars;
Aspose.Email is a commercial option.

## How it compares

| | pstsmith | [outlook-pst-rw](https://github.com/dongs0104/outlook-pst-rs) | [outlook-pst](https://github.com/microsoft/outlook-pst-rs) | [libpff](https://github.com/libyal/libpff) | [PST Builder](https://github.com/ml6719/PST-Builder) | [imap2pst](https://github.com/bXi/imap2pst) | [PSTForge](https://github.com/calculatetech/PSTForge) |
|---|---|---|---|---|---|---|---|
| Language, runtime | Rust, none | Rust, none | Rust, none | C (Python binding) | C#, .NET | C++, Linux | Rust, Linux, libpff |
| Reads PST | ✓ | ✓ | ✓ | ✓ | – | – | via libpff |
| Creates PST | ✓ Unicode | ✓ Unicode | – | – | ✓ Unicode | ✓ Unicode | ✓ Unicode parts |
| Appends to a PST | ✓ | ✓ | – | – | – (new parts) | – (rebuilds) | – |
| Input | EML, MBOX, Maildir, JSONL | API | – | – | API | IMAP | PST, OST |
| Crash-safe commits | ✓ tested | not stated | – | – | checkpoints | not stated | not stated |
| Published Outlook / SCANPST checks | ✓ [details](docs/VERIFICATION.md) | not stated | – | – | claimed (README) | claimed (README) | claimed (README) |
| Writes contacts, calendar | – | – | – | – | ✓ | – | – |
| License | MIT | MIT | MIT | LGPL-3.0+ | MIT | MIT | MIT or Apache-2.0 |

"Claimed (README)" means the project's README says so and we did not reproduce it. Facts as of November 2026, from
each project's README; corrections are welcome.

## Documentation

- [docs.rs/pstsmith](https://docs.rs/pstsmith): the library API.
- [docs/VERIFICATION.md](docs/VERIFICATION.md): what was checked, how, and the known limits.
- [docs/MANIFEST.md](docs/MANIFEST.md): the JSONL input and result formats.
- [docs/UPSTREAM.md](docs/UPSTREAM.md): how pstsmith relates to its upstream projects.
- [CHANGELOG.md](CHANGELOG.md).

## Versions

pstsmith follows [Semantic Versioning](https://semver.org); before 1.0, a minor version may change the API. The
minimum supported Rust version is 1.85, and raising it is a minor change. The manifest and result formats are
versioned by their schema names.

## Contributing

Bug reports and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). Please never attach real
mailboxes to an issue. Security issues: [SECURITY.md](SECURITY.md).

## Acknowledgements

pstsmith stands on [microsoft/outlook-pst-rs](https://github.com/microsoft/outlook-pst-rs) and
[dongs0104/outlook-pst-rs](https://github.com/dongs0104/outlook-pst-rs), whose authors' history this repository keeps;
on the [MS-PST](https://learn.microsoft.com/openspecs/office_file_formats/ms-pst/) and MS-OXCMAIL specifications; on
[mail-parser](https://github.com/stalwartlabs/mail-parser); and on [libpff](https://github.com/libyal/libpff) and
[XstReader](https://github.com/Dijji/XstReader), which read its files back.

## License

[MIT](LICENSE). Microsoft and Outlook are trademarks of the Microsoft group of companies.
