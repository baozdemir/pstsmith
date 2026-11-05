# How pstsmith is verified

A PST writer is only as good as the programs that read its files. pstsmith's output was checked by Microsoft's own
tools (classic Outlook, SCANPST, a Microsoft 365 import), by two independent readers written by other people
(libpff and XstReader), and by its own structural checks. This page says what each check covers, what it found, and
what it does not cover.

Passing every check here does not prove that pstsmith has no bugs. It means none is known; the "Known limits" below
are the ones that are.

## On every change (CI)

| Check | What it covers |
|---|---|
| Unit and integration tests (`cargo test`) on Linux, macOS and Windows | the writer's invariants after every commit (reference counts, B-trees, allocation map pages, table states, heap garbage), the MIME mapping rule by rule, the containers, the command line |
| Fixture byte identity (`crates/pstsmith/tests/fixtures.rs`) on all three systems | every fixture under `tests/fixtures` gives the bytes in `expected.sha256`: same input, same file, on every platform |
| libpff oracle (`tests/oracle/compare_mail.py`) | every mail fixture's PST read back by libpff and compared with Python's `email` reading of the same source: folder, subject, message class, plain and HTML bodies, attachment names and bytes, the number of attached e-mails, recipient addresses, read state; the class of delivery reports against an independent Python statement of MS-OXCMAIL 2.2.3.7; a negative control must fail |
| Structure (`tests/oracle/structure.sh`) | block reference counts and the allocation map against the B-trees; folder counters and every contents-table cell against the messages |
| `cargo clippy -D warnings`, `cargo fmt --check`, `cargo doc -D warnings`, `cargo deny` | lints, formatting, documentation links, dependency licences and advisories |

## Recorded checks of the writer (October-November 2026)

These ran on the writer as it is in this repository. 17 of the 20 checked files were written from the manifest
fixtures in `tests/fixtures/manifest`; the fixtures' outputs are byte-identical to them apart from each store's display
name, which is the output file's name.

| Check | Result |
|---|---|
| **SCANPST** (Inbox Repair Tool of classic Outlook for Windows, Microsoft 365 Apps, Outlook 16.0.20527.20112) on a kit of 20 files: the 17 manifest fixtures and three more test manifests | only minor inconsistencies, with no `!!` or `??` line, on all 20 files; the same on 5 of them after Outlook itself added a message |
| SCANPST after classic Outlook added a message to a 10,000-item and a 500-item file written over many commits | only minor inconsistencies, with no `!!` or `??` line, before and after Outlook's add |
| **Classic Outlook** opening the files | Outlook opened every file and all 500 messages of the 500-item file; its object-model read of all 20 kit files equals the expected content line for line |
| **Microsoft 365 import** (Microsoft Purview, network upload of the 20-file kit into a test tenant's mailbox) | Purview's analysis accepted all 20 files (0 corrupt, 0 analysis failures); every import request transferred the messages Outlook reads from its file (1,171 in all), 0 bad items. Exchange Online held back only what its limits forbid: three 67 MB messages (default receive size 36 MB) and one message with 4,200 named properties (see "Known limits") |
| Microsoft Graph comparison of the imported mailbox with the source | 1,167 messages compared field by field (sender, recipients, read flag, importance, attachments and their bytes, bodies); every difference is explained: Exchange's own handling (a 777-character subject cut to its 255-character limit, an image's alt text in the rendered body, folders differing only in case renamed), the comparison's own baseline (Outlook's object model cannot read the encrypted S/MIME items, so those were compared with the source), or the report subjects described under "Known limits" |
| **Two independent readers** (libpff and XstReader) against the manifests, field by field | 202 of 202 campaign cases |
| **Appending to files Outlook wrote** | Microsoft's `sample1.pst` and four PSTs classic Outlook wrote (empty; folders only; 23 items of many classes; 9 messages): after appends by this writer, pstsmith-check's reference and table checks and a heap audit pass and libpff lists Outlook's own items unchanged plus the new ones. On an earlier writer version, SCANPST found only minor inconsistencies in Outlook's two new PSTs after the append, with logs equal to those of Outlook's own files, and classic Outlook opened every appended message; SCANPST and Outlook were not rerun on this version |
| **Crash safety**: a replay of every on-disk state a crash could leave during real conversions (64 and 1,060 states) | 0 violations: every state opens as the last committed one; the replay's controls still catch planted violations |
| **Out of space** (ENOSPC) on ext4, xfs and tmpfs | 12 of 12 cases refused cleanly, no partial output left |
| **Same bytes on every platform** | the corpus outputs are byte-identical on macOS arm64, Linux arm64 and Linux x86-64 |
| **Memory** (peak RSS of the converter, macOS) | 17.2 MB at 128,000 items in one folder; 13.0 MB for 32,000 items in 1,024 folders; 6.5 MB for an export with a 6 GiB attachment. Growth is bounded by caches with named caps in the code |
| Four independent code reviews | every defect found fixed, each fix with a test that fails on the code before it; a few findings kept as measured bounds or documented conditions instead |

## Recorded checks of the mail input (November 2026)

| Check | Result |
|---|---|
| libpff oracle on the 6 mail fixture cases (EML, Google Takeout, Thunderbird, Apple Mail, CRLF MBOX, Maildir), legacy charsets and Maildir names in modified UTF-7 included | 37 messages, 0 mismatches; the negative control finds 74 |
| XstReader and libpff reading the same mail fixture PSTs | 37 messages, 0 disagreements, attached e-mails, categories, transport headers, search keys and times included |
| Structure checks on the mail fixture PSTs | 6 of 6 pass |
| **Memory** (peak RSS, macOS; the ignored test `mbox_streams_without_reading_whole_file`) | 798 MB for a 2 GiB MBOX whose largest message is 300 MiB: one message is held at a time, and the peak was about 2.5 times the largest message |

The MIME mapping changes nothing in the writer: a message is mapped to the item form of the manifest and then takes
the manifest conversion's path unchanged, so the writer checks above apply to its files too.

## Known limits

- **Not written:** ANSI (Outlook 97-2002) files; cyclic-encoded files (appending to one is refused); contacts,
  calendar items and tasks as their own item types (a calendar invitation arrives as mail with its `.ics` attached);
  TNEF (`winmail.dat`) is kept as an attachment, not decoded; files are never compacted.
- **Not mapped from MIME:** Reply-To (it stays in the original headers, but a reply in Outlook goes to the sender);
  a recipient without an address (it stays in the original headers).
- **Folder names are used as the file system gives them,** without Unicode normalization: a name stored decomposed
  (NFD, as some macOS tools write it) and the same name composed (NFC) become two folders.
- **One file holds at most 10 GB** (`--max-file-size`, the largest PST Purview eDiscovery exports write); `--output-dir`
  writes as many parts as needed.
- **A conversion killed in a way it cannot catch** (SIGKILL, an out-of-memory kill), or stopped by a second Ctrl-C,
  leaves its staging directory, which holds decoded mail, and its partial output behind. A failure removes both, and
  so does `pstsmith convert` when it is stopped by Ctrl-C (on Unix also SIGTERM, SIGQUIT or SIGHUP), also while it
  writes a file (a signal after the last file is complete keeps the whole files, and so does a `--json` result
  line that cannot be written to stdout, which is reported as an error); on Windows only Ctrl-C is handled (Ctrl-Break and closing the console window are not). A library caller stops a conversion with `Converter::set_cancel_flag`.
- **Microsoft 365 import limits seen in the test above,** which apply to any PST:
  - Exchange Online's default maximum receive size is 36 MB (at most 150 MB); larger items are skipped by the import.
  - A mailbox has a quota of named properties (16,384 in all; 3,277 per custom namespace or in PS_PUBLIC_STRINGS;
    1,638 per prefix); a message that needs more is skipped.
  - Exchange compares folder names without case: `Inbox` and `inbox` become one folder (pstsmith keeps them distinct
    and warns).
- **Report messages in a Microsoft 365 import:** delivery and read reports (REPORT.IPM.Note.NDR, .DR, .IPNRN and the
  like) arrive with their subject reduced to its prefix ("Undeliverable:", "Read:"). This is Exchange's handling: a
  PST classic Outlook wrote itself, imported the same way, shows the same, and so does pstsmith's file written with
  Outlook's subject prefix marker; ordinary messages keep their subjects. Outlook and other readers of the PST show the
  whole subject.
