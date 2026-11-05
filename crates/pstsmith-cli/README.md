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
([format](https://github.com/baozdemir/pstsmith/blob/main/docs/MANIFEST.md)). `-o` writes one file of at most 10 GB
(`--max-file-size`); `--output-dir` writes as many parts as needed. An error is the first line on stderr, as JSON; the
exit code says what failed (2 invalid input, 3 an attachment that does not match its recorded size or hash, 4 an
output that exists, 5 a writer or I/O failure). A conversion stopped by Ctrl-C removes what it wrote, reports
`interrupted` and ends by the signal (a shell shows exit status 128 plus its number; on Windows the exit code is
STATUS_CONTROL_C_EXIT). With `--json`, a result line that cannot be written to stdout is an `io` error (exit 5),
and the files, which are complete, are kept.

The library is the [`pstsmith`](https://crates.io/crates/pstsmith) crate; see the
[repository](https://github.com/baozdemir/pstsmith) for how its output was verified.
