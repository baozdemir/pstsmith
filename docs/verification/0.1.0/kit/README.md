# The 20-file kit

SCANPST, classic Outlook and the Microsoft 365 import all checked the same 20 PST files, written on 2026-11-02 by the
research build pstsmith was frozen from. `SHA256SUMS` lists them; the upload to Microsoft 365 checked these hashes
before it started.

17 of the files come from the manifests in `tests/fixtures/manifest`, under the names below. A store's display name is
its file's name, which is why the repository's own fixture outputs (`tests/fixtures/expected.sha256`) have other
hashes. To rebuild the 17 with a pstsmith 0.1.0 binary (built from tag v0.1.0, or from the release) and compare them
with `SHA256SUMS`, from the repository root:

    sh docs/verification/0.1.0/kit/regenerate.sh path/to/pstsmith OUTDIR

`regenerate.log` is the run on 2026-11-08 (macOS 15.6.1, arm64): all 17 are byte-identical to the files that were
checked. You can run SCANPST and Outlook on your copies and compare with [../scanpst](../scanpst/) and
[../outlook](../outlook/).

| Kit file | Manifest in `tests/fixtures/manifest` |
| --- | --- |
| `h-correctness.pst` | `v1-correctness.jsonl` |
| `h-folder-identity.pst` | `v1-folder-identity.jsonl` |
| `h-large_attachment.pst` | `v1-large_attachment.jsonl` |
| `h-all-excluded.pst` | `v2-all-excluded.jsonl` |
| `h-correctness-as-v2.pst` | `v2-correctness-as-v2.jsonl` |
| `h-empty-names.pst` | `v2-empty-names.jsonl` |
| `h-folder-identity-as-v2.pst` | `v2-folder-identity-as-v2.jsonl` |
| `h-folders-only.pst` | `v2-folders-only.jsonl` |
| `h-large-cells.pst` | `v2-large-cells.jsonl` |
| `h-mail-complete.pst` | `v2-mail-complete.jsonl` |
| `h-mail-folders.pst` | `v2-mail-folders.jsonl` |
| `h-meetings.pst` | `v2-meetings.jsonl` |
| `h-named.pst` | `v2-named.jsonl` |
| `h-reports.pst` | `v2-reports.jsonl` |
| `h-smime.pst` | `v2-smime.jsonl` |
| `h-split-many-small.pst` | `v2-split-many-small.jsonl` |
| `h-split.pst` | `v2-split.jsonl` |

The other three, `h-inline-image.pst` (three messages with inline images), `h-interleaved-as-v2.pst` (600 messages
alternating between Inbox and Sent) and `h-r6-A-production.pst` (three messages with empty values: empty bodies,
HTML and attachments), come from test manifests of the research workspace that are not published.
