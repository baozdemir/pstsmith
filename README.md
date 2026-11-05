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
  <img alt="MSRV 1.88" src="https://img.shields.io/badge/MSRV-1.88-orange.svg">
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

- **The writer's files are checked by Microsoft's own tools.** On a 20-file test kit, SCANPST finds only minor
  inconsistencies, with no `!!` or `??` line; classic Outlook opens the files; a Microsoft 365 (Purview) import
  accepted every file with 0 bad items. The mail input's mapping is checked against Python's `email` reading of the
  sources. [How it was verified](docs/VERIFICATION.md).
- **Read back by two independent readers,** libpff and XstReader, field by field.
- **Crash-safe appends.** Every commit is atomic: a crash or a full disk leaves the file at its last committed state.
- **Bounded memory.** The writer streams attachments from disk: 128,000 messages in one folder took 17 MB. Mail
  input is read one message at a time, so memory follows the largest message, not the size of the input.
- **Size-limited parts** (1, 2, 5 or 10 GB) for imports with per-file limits.
- **Same input, same bytes:** the outputs are byte-identical on macOS and Linux (arm64 and x86-64), and CI checks the
  fixtures' hashes on Linux, macOS and Windows.
- **No part dropped.** Every MIME part becomes a body or an attachment and the original headers are kept; what cannot
  be shown as mail (an unreadable message, an attached e-mail nested too deep) is kept as an `.eml` attachment and
  reported.

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
`Name.sbd` / Apple Mail `Name.mbox` folders become folders, Maildirs become folders (Maildir++ names in Dovecot's
modified UTF-7 are decoded). Gmail labels become folders and categories, each message written once. `-o` writes one
file of at most 10 GB (`--max-file-size`), the largest PST Microsoft's own exports write; a larger mailbox needs
`--output-dir`. The decoded messages are staged in a private temporary directory (`--temp-dir`), which needs about as
much free space as the input and is removed when the conversion ends. Stopped by Ctrl-C, SIGTERM or SIGHUP, `convert`
removes it and its partial output; only a kill that cannot be caught (SIGKILL, an out-of-memory kill) leaves them.

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
| Append | Unicode files, unencrypted or permute-encoded (as Outlook writes them), including files Outlook created ([checked](docs/VERIFICATION.md#recorded-checks-of-the-writer-october-november-2026)) |
| Input | EML, MBOX (mboxo/mboxrd, LF or CRLF), Maildir and Maildir++, Thunderbird and Apple Mail folders, JSONL manifest ([format](docs/MANIFEST.md)) |
| Mail | plain and HTML bodies, inline images, attachments, attached e-mails, S/MIME signed and encrypted mail, delivery and read reports, categories, read and draft state, importance, the original headers |

**When not to use pstsmith:** to write ANSI (Outlook 97-2002) files, to read OST files, to convert contacts, calendars
or tasks as such, to write cyclic-encoded or password-protected files, or to recover a damaged PST. For reading,
[libpff](https://github.com/libyal/libpff) and readpst serve well; [PSTForge](https://github.com/calculatetech/PSTForge)
rebuilds damaged PSTs into new ones; [PST Builder](https://github.com/ml6719/PST-Builder) writes contacts and
calendars; Aspose.Email and Redemption are commercial options.

## How it compares

What each tool does, from its own documentation (November 2026). "Claimed" means the project says so and we did not
reproduce it; "not stated" means its documentation does not say; "not checked" marks a commercial product whose
documentation we could not check on that point. Corrections are welcome.

| | Language, runtime | Reads | Creates | Appends to its own files | Appends to Outlook's files | ANSI write | Contacts, calendar, tasks | PST encoding written | Input | OST input |
|---|---|---|---|---|---|---|---|---|---|---|
| **pstsmith** | Rust, none | ANSI, Unicode | Unicode | ✓ | ✓ [checked][v-append] | – | – | permute (Outlook's default) | EML, MBOX, Maildir, JSONL | – |
| [outlook-pst-rw][rw] | Rust, none | [✓ (Microsoft's reader)][rw] | [Unicode][rw] | [✓][rw] | [unencrypted files only][rw] | [–][rw] | not stated | [unencrypted][rw] | API | not stated |
| [microsoft/outlook-pst-rs][ms] | Rust, none | [✓][ms] | [–][ms] | [–][ms] | [–][ms] | – | – | – | – | not stated |
| [libpff][pff] | C (Python binding) | [ANSI, Unicode][pff] | [– (requested)][pff-write] | – | – | – | – | – | – | [✓][pff] |
| [PST Builder][pb] | C#, .NET | [–][pb] | [Unicode][pb] | [– (new parts)][pb] | [–][pb] | [–][pb] | [✓][pb] | [unencoded][pb] | [EML, vCard, iCal, API][pb] | not stated |
| [imap2pst][i2p] | C++, Linux | – | [Unicode][i2p] | [– (rebuilds)][i2p] | – | [–][i2p] | not stated | [unencoded][i2p] | [IMAP][i2p] | – |
| [PSTForge][pf] | Rust, Linux, libpff | [via libpff, damaged files too][pf] | [Unicode parts][pf] | [–][pf] | [–][pf] | not stated | not stated | not stated | [PST][pf] | [– in 1.0][pf] |
| [Aspose.Email][as] | .NET, Java and others | not checked | [Unicode][as-create] | not checked | not checked | [–][as-create] | not checked | not checked | not checked | not checked |
| [Redemption][rd] (Outlook's MAPI) | COM, Windows with Outlook | [through MAPI][rd] | [Unicode, ANSI][rd-store] | [through MAPI][rd] | [through MAPI][rd] | [✓][rd-store] | not checked | not checked | API | not checked |

| | Platforms tested in CI | Crash safety | Compaction | Published checks by other programs | License |
|---|---|---|---|---|---|
| **pstsmith** | Linux, macOS, Windows | atomic commits; every crash state replayed ([details][v]) | – (freed space is reused) | SCANPST, classic Outlook, a Microsoft 365 import, libpff, XstReader ([details][v]) | MIT |
| [outlook-pst-rw][rw] | not stated | not stated | [– (holes not reclaimed)][rw] | not stated | [MIT][rw] |
| [microsoft/outlook-pst-rs][ms] | not stated | – (reader) | – | – | [MIT][ms] |
| [libpff][pff] | not stated | – (reader) | – | – | [LGPL-3.0+][pff] |
| [PST Builder][pb] | not stated | [fsynced checkpoints into new parts][pb] | [– (new files only)][pb] | [claimed: Outlook opens the files; SCANPST shows two advisories][pb] | [MIT][pb] |
| [imap2pst][i2p] | not stated | [rebuilds; a half-written file is not resumed][i2p] | [– (no space reuse)][i2p] | [claimed: Outlook opens the files without a repair prompt][i2p] | [MIT][i2p] |
| [PSTForge][pf] | [Ubuntu 24.04, Debian 13][pf] | [parts renamed into place after validation; restartable mode][pf] | [– (new files only)][pf] | [claimed: clean SCANPST, then Outlook][pf] | [MIT or Apache-2.0][pf] |
| [Aspose.Email][as] | not checked | not checked | not checked | not checked | commercial |
| [Redemption][rd] | not checked | [Outlook's own PST provider][rd] | not checked | not checked | commercial |

[v]: docs/VERIFICATION.md
[v-append]: docs/VERIFICATION.md#recorded-checks-of-the-writer-october-november-2026
[rw]: https://github.com/dongs0104/outlook-pst-rs/blob/main/crates/pst/README.md
[ms]: https://github.com/microsoft/outlook-pst-rs/blob/main/crates/pst/README.md
[pff]: https://github.com/libyal/libpff
[pff-write]: https://github.com/libyal/libpff/issues/116
[pb]: https://github.com/ml6719/PST-Builder
[i2p]: https://github.com/bXi/imap2pst
[pf]: https://github.com/calculatetech/PSTForge
[as]: https://products.aspose.com/email
[as-create]: https://reference.aspose.com/email/net/aspose.email.storage.pst/personalstorage/create
[rd]: https://www.dimastr.com/redemption/home.htm
[rd-store]: https://www.dimastr.com/redemption/rdo/rdosession.htm

## Documentation

- [docs.rs/pstsmith](https://docs.rs/pstsmith): the library API.
- [docs/VERIFICATION.md](docs/VERIFICATION.md): what was checked, how, and the known limits.
- [docs/MANIFEST.md](docs/MANIFEST.md): the JSONL input and result formats.
- [docs/UPSTREAM.md](docs/UPSTREAM.md): how pstsmith relates to its upstream projects.
- [CHANGELOG.md](CHANGELOG.md).

## Versions

pstsmith follows [Semantic Versioning](https://semver.org); before 1.0, a minor version may change the API. The
minimum supported Rust version is 1.88, and raising it is a minor change. The manifest and result formats are
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
