#!/bin/sh
# Structural checks of PST files with pstsmith's own check tools (crates/pstsmith-check): block reference counts and
# the allocation map against the B-trees (pst-ref-check), and every folder's counters and contents-table cell against
# the messages (pst-table-check). Exit 0 only when every file passes both.
#   sh tests/oracle/structure.sh FILE.pst...     (tools from target/release, or PSTSMITH_CHECK_BIN)
set -u
BIN=${PSTSMITH_CHECK_BIN:-target/release}
status=0
for pst in "$@"; do
  if ! "$BIN/pst-ref-check" "$pst" > /dev/null; then
    echo "pst-ref-check failed: $pst"
    status=1
  fi
  if ! "$BIN/pst-table-check" "$pst" | tail -n 1 | grep -q '^RESULT mismatches=0$'; then
    echo "pst-table-check failed: $pst"
    status=1
  fi
done
echo "$# files checked"
exit $status
