"""pstsmith E6: compare, message by message, what Exchange's own MIME reader made of each source .eml (folder E6-MIME,
uploaded by upload_mime.ps1) with what the Purview import made of pstsmith's PST of the same sources (folders E6-PST/...),
both read through Microsoft Graph (graph-read.jsonl). Paired by Message-ID, then subject.

Compared: subject, To/Cc/Bcc (address and name), importance, categories, file attachments (name, inline, SHA-256 of
the bytes), attached e-mails (count), and the plain-text body as Exchange renders it (normalized line endings and
trailing whitespace). Not compared, because Graph's "create message" makes a draft: sender, read state, times.
A difference is classified as pstsmith's (to be fixed with a failing test) or Exchange's (recorded) in README.md.

    python3 -B compare_exchange.py graph-read.jsonl E6-PST-B > compare.txt
"""
import collections
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
GRAPH = sys.argv[1] if len(sys.argv) > 1 else "graph-e6.jsonl"
PST_ROOT = sys.argv[2] if len(sys.argv) > 2 else "E6-PST"
rows = [json.loads(line) for line in (HERE / GRAPH).read_text(encoding="utf-8-sig").splitlines() if line.strip()]
mime = [r for r in rows if r["folder"].split("/")[0] == "E6-MIME"]
pst = [r for r in rows if r["folder"].split("/")[0] == PST_ROOT]


def norm(text):
    return "\n".join(line.rstrip() for line in (text or "").replace("\r\n", "\n").replace("\r", "\n").split("\n")).strip("\n")


def people(row, field):
    return sorted(((r.get("name") or ""), (r.get("address") or "").lower()) for r in row.get(field) or [])


def files(row):
    return sorted(((a.get("name") or ""), bool(a.get("isInline")), a.get("sha256")) for a in row.get("attachments") or [] if a.get("type") == "#microsoft.graph.fileAttachment")


def items(row):
    return sum(1 for a in row.get("attachments") or [] if a.get("type") == "#microsoft.graph.itemAttachment")


totals = collections.Counter()
diffs = []
remaining = list(pst)
for m in mime:
    match = next((p for p in remaining if m.get("internetMessageId") and p.get("internetMessageId") == m["internetMessageId"] and p["subject"] == m["subject"]), None) \
        or next((p for p in remaining if m.get("internetMessageId") and p.get("internetMessageId") == m["internetMessageId"]), None) \
        or next((p for p in remaining if p["subject"] == m["subject"]), None)
    if match is None:
        diffs.append(("missing from the PST import", m["subject"], m.get("internetMessageId")))
        totals["missing"] += 1
        continue
    remaining.remove(match)
    totals["paired"] += 1
    for field, a, b in [
        ("subject", m["subject"], match["subject"]),
        ("to", people(m, "to"), people(match, "to")),
        ("cc", people(m, "cc"), people(match, "cc")),
        ("bcc", people(m, "bcc"), people(match, "bcc")),
        ("importance", m.get("importance"), match.get("importance")),
        ("categories", sorted(m.get("categories") or []), sorted(match.get("categories") or [])),
        ("file_attachments", files(m), files(match)),
        ("item_attachments", items(m), items(match)),
        ("body_text", norm(m.get("body")), norm(match.get("body"))),
    ]:
        if a == b:
            totals["same:" + field] += 1
        else:
            totals["diff:" + field] += 1
            diffs.append((field, m["subject"], str(a)[:300], str(b)[:300]))
for p in remaining:
    diffs.append(("only in the PST import", p["subject"], p.get("internetMessageId")))
    totals["extra"] += 1
print(f"Exchange MIME reader: {len(mime)} messages; Purview import of pstsmith's PST: {len(pst)} messages")
for key in sorted(totals):
    print(f"  {key}: {totals[key]}")
print()
for diff in diffs:
    print(" | ".join(str(x) for x in diff))
sys.exit(0)
