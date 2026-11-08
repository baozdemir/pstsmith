# Appending to files Outlook and Microsoft wrote

The library can append to an existing PST. These runs appended to copies of files that pstsmith did not write.

## 2026-11-05: the writer pstsmith 0.1.0 carries

A probe program built from the frozen writer sources (the writer of pstsmith 0.1.0) appended messages to copies of
five files, on macOS:

| Folder | File (written by) | Before | Appended |
| --- | --- | --- | --- |
| `sample1/` | Microsoft's `sample1.pst` from the PST File Format SDK (pstsdk); permute-encoded, a 55-column contents table | 1 message | 4 messages, with and without Message-ID, sender and times |
| `outlook-new/` | `outlook-empty` (classic Outlook 16.0.20527) | an empty file | 3 messages into a new Inbox |
| `outlook-new/` | `outlook-folders-only` (classic Outlook) | folders Archive and Archive\2026 | the same |
| `outlook-content/` | `outlook-classes` (classic Outlook) | 23 items of many classes | 3 messages into a new Inbox |
| `outlook-content/` | `gt-mail` (classic Outlook) | 9 messages, 5 in the Inbox | 3 messages into the existing Inbox |

Each file was checked before and after the append (`<name>.*` and `<name>-appended.*`):

- `*.ref-check.txt`: `pst-ref-check`, block reference counts and the allocation map against the B-trees;
- `*.table-check.txt`: `pst-table-check`, folder counters and every contents-table cell against the messages;
- `*.heap_audit.json`: an audit of every heap (no garbage, dangling or doubly used allocations, fill levels, free
  counts), with a logical comparison of before and after for `sample1`;
- `*.libpff.txt`: libpff's listing of every folder and message;
- `*.exits`: the exit status of each; `sample1.log` and `outlook-new.log` sum them up.

Every check exits 0 on every copy, before and after. libpff's listing of each appended copy differs from its source
only by the new messages: the files' own items, folders and counts are kept. SCANPST and classic Outlook were not run
on these copies; see the earlier run below.

## 2026-10-31: an earlier writer, with SCANPST and Outlook

On the Windows VM, an earlier version of the writer appended 3 messages to copies of the two new files classic Outlook
had made (`outlook-empty`, `outlook-folders-only`):

- `scanpst-logs/`, `scanpst.jsonl`: SCANPST on Outlook's two files and on the two appended copies, all four "Only minor
  inconsistencies"; the appended files' logs are line for line those of the files they were made from.
- `*.outlook.jsonl`: classic Outlook's object model reads both appended files.
- `*.open.jsonl`: Outlook opens every message of both in its own window, 3 of 3 each.
- `libpff-listing.txt`: libpff lists Outlook's folders and the 3 new messages in the Inbox.

The writer changed after this run, which is why the run above repeats the structural checks on its current version.
