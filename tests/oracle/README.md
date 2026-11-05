# Oracles

Checks that share no code with pstsmith.

- `compare_mail.py PSTSMITH`: converts every mail fixture, reads the PSTs back with libpff (`pip install libpff-python`)
  and compares them field by field with what Python's standard `email` package reads from the same sources: folder,
  subject, class, plain and HTML body, attachments (names and SHA-256 of their bytes), attached e-mails, recipients and
  read state. Delivery reports are checked against `report_reference.py`, an independent statement of MS-OXCMAIL
  2.2.3.7 in Python. `--negative-control` pairs every source with the next case's PST and must report mismatches in each of those pairs.
- `structure.sh FILE.pst...`: block reference counts, the allocation map, folder counters and every contents-table
  cell, with the tools of `crates/pstsmith-check`.
- `report_reference.py`: run it alone for its self-check.

The maintainers also read the outputs with XstReader (.NET) and compare it with libpff field by field; that run and
the checks with SCANPST, classic Outlook and a Microsoft 365 import are described in `docs/VERIFICATION.md`.
