# Exchange Online's MIME reader against pstsmith's mail input

The 46 messages of the mail fixtures (`tests/fixtures/mail`: EML, Google Takeout, Thunderbird, Apple Mail, CRLF MBOX
and Maildir) went into one test mailbox twice, on 2026-11-06:

1. as MIME, through Microsoft Graph's "create message" in MIME format, so that Exchange Online's own MIME reader parsed
   each source (folder `E6-MIME`);
2. as pstsmith's PST of the same sources, through a Microsoft Purview network import (folder `E6-PST-B`). The PST was
   written by main at 20afb9d, which has the code of v0.1.0 (the later commits change only documentation). Purview
   reported the job completed: 46 imported, 0 skipped.

Both folders were read back through Microsoft Graph (`graph-read.jsonl`) and compared message by message by
`compare_exchange.py`: subject, To, Cc and Bcc (address and name), importance, categories, file attachments (name,
inline, SHA-256 of the bytes), attached e-mails, and the plain-text body as Exchange renders it. Sender, read state and
times are not compared, because Graph's "create message" makes a draft. `compare.txt` is the output; run from this
folder, `python3 -I compare_exchange.py graph-read.jsonl E6-PST-B` reproduces it byte for byte.

## Result

- **In the mailbox:** 46 of 46 from each path.
- **Paired: 42.** Subject, To, Cc, Bcc, importance and attached e-mails are equal in all 42.
- **Every other difference, and why:**
  - *Categories (3 messages):* Exchange's MIME reader does not turn Gmail labels or a Keywords header into categories;
    pstsmith does.
  - *The S/MIME signed message's `smime.p7m`:* pstsmith's attachment is the signed entity of the source, byte for byte,
    and it survives the import unchanged; Exchange's MIME reader rewrites its header.
  - *Delivery and read reports (body and one attachment):* Exchange regenerates a report's body from its properties;
    pstsmith keeps the source's text and its machine-readable part as an attachment.
  - *Attachments without a name:* Exchange calls them `ATT00001.ics` or `.png`, pstsmith `attachment-1.ics` or `.png`;
    the bytes are the same.
  - *`winmail.dat`:* pstsmith keeps TNEF as an attachment (a documented limit).
  - *4 unpaired on each side:* three reports and a file that is not a message have no Message-ID in their sources, so
    Exchange assigns one on each import, and the report subjects arrive reduced to their prefix (see
    [../microsoft-365-import](../microsoft-365-import/README.md#report-subjects)).

## The defect this check found

An earlier run of the same check (main at f32606e, before the release) found one: a Takeout message whose Gmail label
held a comma ("Projects, 2026") was counted by the import but left out of the mailbox, since Outlook and Exchange
separate categories with commas and semicolons. Commit 20afb9d writes each comma and semicolon of a category name as a
space, with a warning, and this run's message arrived with "Projects 2026".
