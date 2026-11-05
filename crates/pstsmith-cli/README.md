# pstsmith (command line)

Convert EML files, MBOX files (Google Takeout, Thunderbird, Apple Mail) and Maildir directories into Outlook PST files,
on Linux, macOS and Windows, without Outlook.

```sh
cargo install pstsmith-cli
pstsmith convert "Takeout/Mail/All mail Including Spam and Trash.mbox" -o gmail.pst
pstsmith convert ~/Maildir --output-dir parts --max-file-size 10GB
pstsmith --help
```

`pstsmith --manifest ITEMS.jsonl --output FILE.pst` converts a JSONL manifest of messages
([format](https://github.com/baozdemir/pstsmith/blob/main/docs/MANIFEST.md)). Errors are one JSON line on stderr; the
exit code says what failed (2 invalid input, 3 an attachment that does not match its recorded size or hash, 4 an output
that exists, 5 a writer or I/O failure).

The library is the [`pstsmith`](https://crates.io/crates/pstsmith) crate; see the
[repository](https://github.com/baozdemir/pstsmith) for how its output was verified.
