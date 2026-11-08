# Verification records for pstsmith 0.1.0

This folder holds the raw output behind the checks in [docs/VERIFICATION.md](../../VERIFICATION.md): the logs and
result files the tools wrote, and the scripts that compared them. Each folder's README says what was checked, when,
with which build and tools, and what came out; where the check can be run again from this repository, it says how.

| Folder | Check |
| --- | --- |
| [kit](kit/) | The 20 PST files that SCANPST, classic Outlook and the Microsoft 365 import checked, and how to rebuild 17 of them with pstsmith 0.1.0 |
| [scanpst](scanpst/) | SCANPST on the kit, after Outlook added messages, and on two large files |
| [outlook](outlook/) | Classic Outlook reading every folder and message of the kit, and opening 500 messages |
| [microsoft-365-import](microsoft-365-import/) | A Purview import of the kit into Exchange Online, compared through Microsoft Graph |
| [exchange-mime](exchange-mime/) | The mail input against Exchange Online's own MIME reader |
| [two-readers](two-readers/) | libpff and XstReader reading the same files, field by field |
| [mail-oracle](mail-oracle/) | The CI oracle: libpff against Python's `email` package, its negative control, and the structure checks |
| [append-to-outlook-files](append-to-outlook-files/) | Appending to PSTs that Outlook and Microsoft wrote |
| [crash-safety](crash-safety/) | 500 kills of a process appending to a PST, and what each left |
| [out-of-space](out-of-space/) | Appends and conversions on a disk that fills up |
| [same-bytes](same-bytes/) | The same output bytes on macOS and Linux, and from all seven release targets |
| [memory](memory/) | Peak memory of the converter |

## Which build each check ran on

pstsmith grew out of a private research workspace. Its writer and converter were frozen there on 2026-11-05 and moved
into this repository unchanged, so several checks ran on research builds before the release. pstsmith 0.1.0 writes
the same bytes as those builds: the 17 kit files it can rebuild from this repository's manifests are byte-identical
to the files those checks used ([kit/regenerate.log](kit/regenerate.log)), and so are the 36 outputs in
[same-bytes](same-bytes/).

| Checks | Build | Date | Machine |
| --- | --- | --- | --- |
| kit rebuild, mail oracle, structure checks, two readers, same bytes on macOS, memory, crash safety, out of space | v0.1.0 (tag, commit cd027e8) | 2026-11-08 | macOS 15.6.1, Apple M4 Pro |
| SCANPST, classic Outlook | the 20 kit files, written by the research build of that day | 2026-11-02 | Windows Server 2025 VM on Google Cloud, Microsoft 365 Apps 16.0.20527 |
| Microsoft 365 import, Graph comparison | the same 20 files | 2026-11-04 | Microsoft Purview and Exchange Online, a test tenant |
| Exchange's MIME reader | main at 20afb9d (the code of v0.1.0; later commits change only documentation) | 2026-11-06 | the same test tenant |
| same bytes on Linux | the research builds of those days | 2026-11-02 and 11-03 | Linux arm64 (Docker), Linux x86-64 (Google Cloud VM) |
| appends to Outlook's files | a probe built from the frozen writer sources | 2026-11-05 (an earlier writer on 2026-10-31) | macOS (the earlier run also on the Windows VM) |

## What was changed in the records

The files are as the tools wrote them, except:

- local paths are replaced by `<workspace>`, `<scratch>`, `<pstsmith>` and `<home>`;
- the test tenant's domain and mailbox are replaced by `tenant.example` and `user@tenant.example`, and the Exchange
  server name inside Message-IDs that Exchange assigned by `MAILBOXSERVER` (its region by `region`);
- in paths, folder names of the research workspace are left out or replaced by this folder's names (`kit`);
- the two comparison scripts, `microsoft-365-import/compare_graph.py` and `exchange-mime/compare_exchange.py`, read
  their inputs from this folder. Run from their folders, they reproduce `graph-compare.txt` and `compare.txt` byte
  for byte.

The replacements were made by one script, on the bytes, so no file's encoding changed (SCANPST writes its logs in the
Windows code page). `SHA256SUMS` lists every other file in this folder.

## Not published here

- **PST files.** 17 of the 20 kit files are rebuilt from this repository ([kit](kit/)). The three others, the two
  large files and the files Outlook wrote come from test manifests or a test mailbox that are not published.
- **The 202-case campaign** that an earlier version of VERIFICATION.md cited for the two readers. It ran in a research
  harness that also tested software that is not part of pstsmith. [two-readers](two-readers/) has fresh runs of the
  same two readers on pstsmith 0.1.0's own files instead.
- **The crash replay and out-of-space runs** that an earlier version of VERIFICATION.md cited. They tested a
  publication step of other software built on the writer, not pstsmith alone. [crash-safety](crash-safety/) and
  [out-of-space](out-of-space/) have runs of pstsmith 0.1.0 itself instead.
- **The code reviews**, and screenshots of Outlook and Purview, which show the test account.
