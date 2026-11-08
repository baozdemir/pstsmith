# The mail oracle and the structure checks

These are the checks of the `oracle` job in `.github/workflows/ci.yml`, which runs on every change. GitHub keeps CI
logs for a limited time, so this is a run of the same commands on pstsmith 0.1.0, saved here: 2026-11-08, macOS
15.6.1, arm64, Python 3.13, libpff 20260926 (the version CI installs; this build adds one method to the Python
binding, which these scripts do not use).

| Log | Command | Result |
| --- | --- | --- |
| `compare-mail.log` | `python tests/oracle/compare_mail.py target/release/pstsmith` | 46 messages compared, 0 mismatches |
| `compare-mail-negative-control.log` | the same with `--negative-control` | 92 mismatches, in all 8 pairs |
| `report-reference.log` | `python tests/oracle/report_reference.py` | self-check passed |
| `structure.log` | `pstsmith convert` of the 8 mail fixtures, then `sh tests/oracle/structure.sh out/*.pst` | 8 files checked, exit status 0 |

- **`compare_mail.py`** converts every mail fixture and reads the PSTs back with libpff. It compares them field by
  field with what Python's standard `email` package reads from the same sources: folder, subject, message class, plain
  and HTML body, attachments (names and SHA-256 of their bytes), attached e-mails, recipients and read state. The
  class of a delivery or read report is checked against `report_reference.py`, an independent statement of MS-OXCMAIL
  2.2.3.7 in Python.
- **The negative control** pairs each source with the next case's PST and must find mismatches in every pair; an
  oracle that cannot fail proves nothing.
- **`structure.sh`** checks every block reference count and the allocation map against the B-trees
  (`pst-ref-check`), and each folder's counters and every contents-table cell against the messages
  (`pst-table-check`).

Repeat it with the commands above from the repository root, after `pip install libpff-python==20260926` and
`cargo build --workspace --release --locked`.
