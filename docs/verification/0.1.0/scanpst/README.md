# SCANPST

SCANPST.EXE is the Inbox Repair Tool that ships with classic Outlook for Windows. It reads every structure of a PST
file and reports what it would repair; in its log it marks the problems it finds with `!!` and `??`.

- **Where and when:** a Windows Server 2025 VM on Google Cloud, 2026-11-02, with Microsoft 365 Apps. SCANPST.EXE's file
  version is 16.0.20527.20112 (`kit.jsonl`); the log header prints 16.0.20527.20088.
- **How:** a PowerShell script started SCANPST on one file at a time, pressed Start, recorded the result dialog's text
  and the log, and closed the tool without repairing anything. The `.jsonl` files hold the dialog text for each file:
  the verdict, and the folders and items SCANPST found.

## Results

| Folder | Files | Verdict | Lines marked `!!` or `??` |
| --- | --- | --- | --- |
| `kit/` | the 20 kit files ([../kit](../kit/)) | 20 of 20: "Only minor inconsistencies were found in this file. Repairing the file is optional." | none |
| `after-outlook-add/` | copies of 5 kit files after classic Outlook added one message to each | 5 of 5: the same | none |
| `large-files/` | a file of 10,000 messages and one of its first 500, both written with a commit every 7 messages, as written and after Outlook added a message | 4 of 4: the same | none |

The five files of `after-outlook-add/` are `h-split`, `h-mail-complete` and `h-large_attachment`, which span more
than 128 allocation-map ranges and so have free-map pages, and `h-correctness` and `h-meetings`. Outlook added each
message through its object model to a copy: a new item, saved and moved into the Inbox, never sent.

For comparison, `large-files/outlook-control-gt-fmap.log` is SCANPST's log of a 56 MB file that classic Outlook wrote
itself on the same VM (12 messages with a 4 MB attachment each). It gets the same verdict, and its log has one `??`
line: `Couldn't find BBT entry in the RBT`.

## Repeat it

Rebuild 17 of the kit files with [../kit/regenerate.sh](../kit/regenerate.sh) and run SCANPST on them. The two large
files come from test manifests that are not published.
