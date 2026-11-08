#!/bin/sh
# Write the 17 kit files that come from tests/fixtures/manifest under the names they were checked with, and compare
# them with SHA256SUMS. Run from the root of the repository at tag v0.1.0:
#   sh docs/verification/0.1.0/kit/regenerate.sh PATH/TO/pstsmith OUTDIR
# OUTDIR must not exist. The three other kit files come from manifests that are not published.
set -eu
BIN=$1
OUT=$2
ROOT=$(pwd)
M="$ROOT/tests/fixtures/manifest"
SUMS="$ROOT/docs/verification/0.1.0/kit/SHA256SUMS"
[ -f "$M/files/large-64MiB.bin" ] || python3 tests/fixtures/generate.py
mkdir "$OUT"
for manifest in "$M"/*.jsonl; do
  stem=$(basename "$manifest" .jsonl)
  name="h-${stem#v[12]-}"
  sed "s#@FIXTURES@#$M#g" "$manifest" > "$OUT/$name.manifest.jsonl"
  case $stem in
    v2-*) "$BIN" --manifest "$OUT/$name.manifest.jsonl" --output "$OUT/$name.pst" --excluded "$OUT/$name.excluded.jsonl" > /dev/null ;;
    *) "$BIN" --manifest "$OUT/$name.manifest.jsonl" --output "$OUT/$name.pst" > /dev/null ;;
  esac
done
cd "$OUT"
if command -v shasum > /dev/null; then check="shasum -a 256 -c"; else check="sha256sum -c"; fi
written=$(ls ./*.pst | sed 's#^\./##' | tr '\n' '|' | sed 's/|$//')
grep -E "  ($written)\$" "$SUMS" > expected.sha256
$check expected.sha256
echo "$(ls ./*.pst | wc -l | tr -d ' ') files written; not written here: $(grep -vE "  ($written)\$" "$SUMS" | awk '{print $2}' | tr '\n' ' ')"
