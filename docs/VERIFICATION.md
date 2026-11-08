# How pstsmith is verified

A PST writer is only as good as the programs that read its files. pstsmith's output was checked by Microsoft's own
tools (classic Outlook, SCANPST, a Microsoft 365 import), by two independent readers written by other people
(libpff and XstReader), and by its own structural checks. This page says what each check covers, what it found, and
what it does not cover.

Passing every check here does not prove that pstsmith has no bugs. It means none is known; the "Known limits" below
are the ones that are.

Most rows below link to their records in [verification/0.1.0](verification/0.1.0/): the raw output and, where the
check can be run again from this repository, how.

## On every change (CI)

| Check | What it covers |
|---|---|
| Unit and integration tests (`cargo test`) on Linux, macOS and Windows | the writer's invariants after every commit (reference counts, B-trees, allocation map pages, table states, heap garbage), the MIME mapping rule by rule, the containers, the command line |
| Fixture byte identity (`crates/pstsmith/tests/fixtures.rs`) on all three systems | every fixture under `tests/fixtures` gives the bytes in `expected.sha256`: same input, same file, on every platform |
| libpff oracle (`tests/oracle/compare_mail.py`) | every mail fixture's PST read back by libpff and compared with Python's `email` reading of the same source: folder, subject, message class, plain and HTML bodies, attachment names and bytes, the number of attached e-mails, recipient addresses, read state; the class of delivery reports against an independent Python statement of MS-OXCMAIL 2.2.3.7; a negative control must fail |
| Structure (`tests/oracle/structure.sh`) | block reference counts and the allocation map against the B-trees; folder counters and every contents-table cell against the messages |
| `cargo clippy -D warnings`, `cargo fmt --check`, `cargo doc -D warnings`, `cargo deny` | lints, formatting, documentation links, dependency licences and advisories |

## Recorded checks of the writer (October-November 2026)

Several of these ran before the release, on builds of the research workspace pstsmith came from; its writer moved into
this repository unchanged, and pstsmith 0.1.0 writes the same bytes as those builds for every file of theirs it can
rebuild from this repository
([which build each check ran on](verification/0.1.0/README.md#which-build-each-check-ran-on)). 17 of the 20 checked
files were written from the manifest fixtures in `tests/fixtures/manifest`; the fixtures' outputs are byte-identical to
them apart from each store's display name, which is the output file's name.

| Check | Result |
|---|---|
| **SCANPST** (the Inbox Repair Tool of classic Outlook for Windows, Microsoft 365 Apps; SCANPST.EXE 16.0.20527.20112) on a kit of 20 files: the 17 manifest fixtures and three more test manifests ([records](verification/0.1.0/scanpst/)) | only minor inconsistencies, with no `!!` or `??` line, on all 20 files; the same on 5 of them after Outlook itself added a message |
| SCANPST after classic Outlook added a message to a 10,000-item and a 500-item file written over many commits ([records](verification/0.1.0/scanpst/)) | only minor inconsistencies, with no `!!` or `??` line, before and after Outlook's add |
| **Classic Outlook** opening the files ([records](verification/0.1.0/outlook/)) | Outlook opened all 20 kit files and read every folder and message through its object model (1,171 messages; for the three S/MIME-wrapped messages, two encrypted and one a signed receipt, it returns only the subject, class and read state), and opened each of the 500 messages of the 500-item file in its own window, with no error |
| **Microsoft 365 import** (Microsoft Purview, network upload of the 20-file kit into a test tenant's mailbox; [records](verification/0.1.0/microsoft-365-import/)) | Purview's analysis accepted all 20 files (0 corrupt, 0 analysis failures); every import request transferred the messages Outlook reads from its file (1,171 in all), 0 bad items. Exchange Online held back only what its limits forbid: three 67 MB messages (default receive size 36 MB) and one message with 4,200 named properties (see "Known limits") |
| Microsoft Graph comparison of the imported mailbox with Outlook's read of the same files ([records](verification/0.1.0/microsoft-365-import/README.md#graph)) | 1,167 messages paired and compared (subject, sender, recipients, read flag, importance, attachments and their bytes, plain-text body); every difference is explained: Exchange's own handling (a 777-character subject cut to its 255-character limit, the report subjects described under "Known limits", plain-text bodies that Exchange renders itself), the baseline (for the three S/MIME-wrapped items Outlook's object model returns no sender, recipients or attachments), the read script (it did not request inline attachments), and Message-IDs that two test messages share |
| **Two independent readers** (libpff and XstReader) reading the 17 kit files pstsmith 0.1.0 rebuilds, compared with each other field by field ([records](verification/0.1.0/two-readers/)) | 565 messages, 2 disagreements: in two files a message with an empty subject, which libpff reads as empty, as the manifest has it, and XstReader shows as its placeholder `<No subject>` |
| **Appending to files Outlook wrote** ([records](verification/0.1.0/append-to-outlook-files/)) | Microsoft's `sample1.pst` and four PSTs classic Outlook wrote (empty; folders only; 23 items of many classes; 9 messages): after appends by this writer, pstsmith-check's reference and table checks and a heap audit pass and libpff lists Outlook's own items unchanged plus the new ones. On an earlier writer version, SCANPST found only minor inconsistencies in Outlook's two new PSTs after the append, with logs equal to those of Outlook's own files, and classic Outlook opened every appended message; SCANPST and Outlook were not rerun on this version |
| **A process killed while it appends** (SIGKILL at 500 random moments while a program appends batches of messages through the library; macOS, APFS; [records](verification/0.1.0/crash-safety/)) | 500 of 500. Each file held a committed state: the batches acknowledged before the kill, or those and the batch being written. As the kill left it, each passed the table check, and the reference and allocation-map check when its header marked the map valid (317 left it marked invalid; the writer rebuilds such a map when it opens the file, as MS-PST 2.6.1.3.7 requires). The writer then opened every file again and appended, and both checks passed. A killed process keeps what it wrote; a power cut was not tested |
| **Out of space** (a 16 MB HFS+ disk image on macOS, with 0 to 1.2 MB free for the appends and 1 to 12 MB for the conversions; [records](verification/0.1.0/out-of-space/)) | 100 of 100 appends stopped with `No space left on device` and left the file at its last commit, passing both checks; once space was freed, the writer opened each file again and appended. 6 of 6 `pstsmith convert` runs, staging on that disk or on another, exited with status 5 and left nothing behind |
| **Same bytes on every platform** ([records](verification/0.1.0/same-bytes/)) | the 36 outputs of 21 test manifests are byte-identical from pstsmith 0.1.0 on macOS arm64 and from the research build on Linux arm64 and Linux x86-64; the 8 mail fixture outputs are byte-identical from the release builds of all seven targets (Linux x86-64 and arm64, glibc and musl; macOS arm64 and x86-64; Windows x86-64), and the release workflow checks every target's build so before it is published |
| **Memory** (peak RSS of the converter, macOS; [records](verification/0.1.0/memory/)) | 15.8 MB at 128,000 items in one folder; 15.3 MB for 32,000 items in 1,024 folders (the higher of two runs each); 7.6 MB for four messages with a 1.5 GiB attachment each (the highest of three runs). Growth is bounded by caches with named caps in the code |
| Four independent code reviews of the writer | every defect found fixed, each fix with a test that fails on the code before it; a few findings kept as measured bounds or documented conditions instead |

## Recorded checks of the mail input (November 2026)

| Check | Result |
|---|---|
| libpff oracle ([records](verification/0.1.0/mail-oracle/)) on the 8 mail fixture cases (EML, Google Takeout, Thunderbird, Apple Mail as a folder tree, as one mailbox and as its mbox file, CRLF MBOX, Maildir), legacy charsets, Maildir names in modified UTF-7, the MBOX separator forms of Thunderbird, Google Takeout, mutt, UW c-client, System V and UUCP, a Date-style one and a body line that must not split included | 46 messages, 0 mismatches; the negative control finds 92, in each of the 8 pairs of a case with the next case's PST |
| XstReader and libpff reading the same mail fixture PSTs ([records](verification/0.1.0/two-readers/)) | 46 messages, 0 disagreements, attached e-mails, categories, transport headers, search keys and times included |
| Structure checks on the mail fixture PSTs ([records](verification/0.1.0/mail-oracle/)) | 8 of 8 pass |
| Ten independent code reviews of the release branch (fresh reviewers, read only: the whole branch, then each fix pass) | every defect found fixed, each fix with a test that fails on the code before it; the tenth review found no critical or important problem |
| **Exchange Online's own MIME reader against a Microsoft 365 import of pstsmith's PST** (the 46 mail fixture messages given to Exchange as MIME through Graph, and as pstsmith's PST through Purview, into one test mailbox; both read back through Graph; [records](verification/0.1.0/exchange-mime/)) | the import holds 46 of 46; the 42 messages both sides could pair have the same subject, recipients, importance and attached e-mails. Every other difference is explained: Exchange's MIME reader does not turn Gmail labels or Keywords into categories; it rewrites an S/MIME signed message's header, while pstsmith keeps the signed bytes and they survive the import unchanged; it regenerates a report's body, while pstsmith keeps the source's text and report part; unnamed attachments get other names (same bytes). The first run found one defect, fixed before this release: a message whose category held a comma was counted as imported but left out of the mailbox |
| **The npm package as a user installs it** (a release dry run: the `pstsmith` tarball with its platform package in an empty project, no `PSTSMITH_BINARY`) | on Linux x86-64 with glibc (Node.js 24) and with musl (Alpine, Node.js 20), macOS arm64 and Windows x86-64, the package finds its platform's binary and converts the EML fixture to the expected bytes |
| **Memory** (peak RSS, macOS; the ignored test `mbox_streams_without_reading_whole_file`; [records](verification/0.1.0/memory/)) | 792 MB for a 2 GiB MBOX whose largest message is 300 MiB: one message is held at a time, and the peak was about 2.5 times the largest message |

The MIME mapping changes nothing in the writer: a message is mapped to the item form of the manifest and then takes
the manifest conversion's path unchanged, so the writer checks above apply to its files too.

## Known limits

- **Not written:** ANSI (Outlook 97-2002) files; cyclic-encoded files (appending to one is refused); contacts,
  calendar items and tasks as their own item types (a calendar invitation arrives as mail with its `.ics` attached);
  TNEF (`winmail.dat`) is kept as an attachment, not decoded; files are never compacted.
- **Category names** from Gmail labels and Keywords headers have each comma and semicolon written as a space (with a
  warning), since Outlook and Exchange separate categories with them; a manifest's `categories` are written as given.
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
