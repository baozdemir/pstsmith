# Classic Outlook

On the same VM and day as [../scanpst](../scanpst/) (Windows Server 2025, Microsoft 365 Apps, classic Outlook
16.0.20527; its object model reports `16.0.0.20527`), a PowerShell script opened each PST in classic Outlook and read
it through Outlook's object model.

- **`read/`:** one file per kit file. The first line names the reader and the store; then one JSON line per folder and
  per message: message class, Message-ID, subject, sender, recipients, importance, read state, sent time, size, body
  and HTML (length and SHA-256), and attachments, down to the deepest attached e-mail. Outlook opened all 20 files
  and read every folder and message; for the three S/MIME-wrapped messages of `h-smime` (two encrypted, one a signed
  receipt) the object model returns only the subject, class and read state.
- **`open-500-messages.jsonl`:** each of the 500 messages of the 500-message file in [../scanpst](../scanpst/)
  (`large-files/`) opened in its own Outlook window: 500 of 500 opened, with no error and no dialog.

The number of messages Outlook read from each file is in
[../microsoft-365-import/expected-from-outlook-read.json](../microsoft-365-import/expected-from-outlook-read.json),
1,171 in all; the Microsoft 365 import transferred exactly that many from each file, and its Graph comparison pairs
every imported message with its line here.

## Repeat it

Rebuild 17 of the kit files with [../kit/regenerate.sh](../kit/regenerate.sh), open them in classic Outlook and
compare what it shows with `read/`.
