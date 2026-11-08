# Microsoft 365 import

Microsoft Purview imports PST files into Exchange Online mailboxes. On 2026-11-04 the 20 kit files
([../kit](../kit/)) were uploaded to a test tenant through Purview's network upload and imported into one mailbox,
each file into its own folder (`/E5-315-<file>`). The mailbox was then read back through Microsoft Graph and compared
with what classic Outlook read from the same files ([../outlook](../outlook/)).

## Purview

- **Analysis:** Purview's own reader analyzed the 20 files before the import. Its job page showed all 20 complete, 0
  skipped as corrupt, 0 as encrypted and 0 for analysis failure. Purview offers no export of that page; this is a
  note of what it showed.
- **`import-requests.csv`:** one import request per file. `ItemsTransferred` equals the messages Outlook read from
  that file (`expected-from-outlook-read.json`) for all 20 files, 1,171 in all, and `BadItemsEncountered` is 0
  everywhere.
- **`import-report.txt`:** why four requests did not end as `Completed`. Each is an Exchange Online limit, not a
  problem of the file:
  - `h-large_attachment`, `h-mail-complete` and `h-split` (`CompletedWithSkippedItems`): each holds one message of
    about 67 MB, with a 64 MiB attachment. The mailbox's maximum receive size was 36 MB, the default.
  - `h-named` (`Failed`): its message "Many names" has 4,200 named properties in one namespace, over Exchange Online's
    quota (3,277 per custom namespace, 1,638 per name prefix). The file's three other messages arrived.
- **`import-folders.csv`:** the folders and item counts in the mailbox after the import. Exchange compares folder
  names without case, so the case-differing folders of `h-folder-identity` (`inbox` and `INBOX`, for example) arrive
  renamed (`inbox_0`).

## Graph

`graph-messages.jsonl` is the mailbox read with Microsoft Graph PowerShell (delegated `Mail.Read`). `compare_graph.py`
pairs each message with its line in [../outlook/read](../outlook/read/), by Message-ID within each file, and compares
sender, recipients, read flag, importance, attachments (names and SHA-256 of their bytes) and the plain-text body.
`graph-compare.txt` is its output; run from this folder, `python3 -I compare_graph.py` reproduces it byte for byte.

- **Paired:** 1,167 messages, the 1,171 less the three large messages and the named-property one.
- **Equal:** read flag 1,167 of 1,167; attached e-mails 1,167; importance 1,164; Bcc and Cc 1,164; To 1,162;
  sender 1,163; attachment bytes 1,159; attachment names 1,153; subject 1,153.
- **The differences, by kind** (each is listed by file in `graph-compare.txt`):
  - *Exchange's handling.* The 777-character subject (it is in five files) is cut to Exchange's 255-character limit.
    The eight report messages of `h-reports` arrive with their subject reduced to its prefix ("Undeliverable:",
    "Read:"), see below. A plain-text body is Exchange's own rendering of the message, so its SHA-256 equals
    Outlook's text in 414 messages and differs in 750.
  - *The baseline.* For the three S/MIME-wrapped messages of `h-smime` (two encrypted, one a signed receipt), classic
    Outlook's object model returns no sender, recipients, importance or attachments, so its read of them has none to
    compare with. For the attachment whose name has 1,900 characters (`h-large-cells`), Outlook's read has an empty name
    and no bytes; Graph returns the name and the bytes, and both equal the manifest's. For a message with an empty
    sender (`h-mail-folders`), Outlook gives an empty string and Graph `null`.
  - *The read script.* Graph reports `hasAttachments` false for a message whose attachments are all inline, and the
    script did not ask for those messages' attachments, so inline attachments (`logo.png`, the hidden `small.bin`)
    show as missing.
  - *The pairing.* Some test suites reuse a Message-ID. In `h-reports` two messages share one and are paired
    crosswise, so their To lines appear swapped. In `h-split` the 64 MiB message that is not in the mailbox shares its
    Message-ID with "Split 22", so the script pairs the two: the subject and attachment differences there are that
    pairing, and the message reported missing is the 64 MiB one's partner.

## Report subjects

Delivery and read reports (REPORT.IPM.Note.NDR, .DR, .IPNRN and the like) arrive in the mailbox with their subject
reduced to its prefix. A follow-up import on 2026-11-05 put a PST that classic Outlook wrote itself through the same
path, and its reports showed the same in Outlook on the web; so did the kit's reports written with Outlook's subject
prefix marker. This is how Exchange stores imported reports; Outlook and other readers of the PST show the whole
subject. That follow-up's records are not published: Outlook's PST and the screenshot come from the test mailbox.
