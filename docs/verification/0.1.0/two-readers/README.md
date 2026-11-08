# Two independent readers

pstsmith's own reader shares its understanding of the format, including any misunderstanding. So its files are also
read by two readers written by other people, and the two are compared with each other field by field:

- **XstReader** (C#, [github.com/iluvadev/XstReader](https://github.com/iluvadev/XstReader) at commit `da80989`, with
  three local read-only changes: access to the named-property map, releasing a message's contents on the calling
  thread, and reading an index record of a B-tree on the heap with the key size its header gives), through a small
  program that prints each message as one JSON line;
- **libpff** 20260926 (C, [github.com/libyal/libpff](https://github.com/libyal/libpff)), with one method added to its
  Python binding to read attached e-mails.

The fields compared for each message: message class, Message-ID, subject, body and HTML (SHA-256), flags, importance,
the submit, delivery, creation and modification times, sender, To and Cc lines, categories, transport headers, search
key, the recipients, the file attachments (name, SHA-256, Content-ID, MIME type) and the attached e-mails, recursively.

Both runs below are of pstsmith 0.1.0 on 2026-11-08, macOS 15.6.1, arm64. The reader programs and the comparison
script are not in this repository.

## `mail-fixtures.log`: the 8 mail fixture outputs

The script converts every mail fixture (`tests/fixtures/mail`) with pstsmith 0.1.0; the eight PSTs are byte-identical
to `tests/fixtures/expected.sha256`. **46 messages read by both readers, 0 disagreements.**

## `kit-files.log`: the 17 kit files

The 17 files [../kit/regenerate.sh](../kit/regenerate.sh) writes, the files SCANPST, Outlook and the Microsoft 365
import checked. **565 messages read by both readers, 2 disagreements**, both the same: one message of `h-correctness`
and of `h-correctness-as-v2` has an empty subject. libpff reads it as empty, as the manifest has it; XstReader shows
`<No subject>`, the text its own code puts in place of an empty subject (`XstReader.Api/LTP.cs`).
