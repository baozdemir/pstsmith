# Changelog

All notable changes to the pstsmith command line are recorded here, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The library's changes are in the repository's
[CHANGELOG.md](../../CHANGELOG.md).

## [Unreleased]

### Added

- `pstsmith convert INPUT...` for EML files, MBOX files, Maildir directories and Thunderbird or Apple Mail folders,
  with `-o`/`--output`, `--output-dir` and `--max-file-size`, `--excluded`, `--temp-dir` and `--json`.
- `pstsmith --manifest ITEMS.jsonl` for the JSONL manifest format, `--help` and `--version`.
