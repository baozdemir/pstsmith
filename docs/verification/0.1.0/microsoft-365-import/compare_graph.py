"""E5: compare what Microsoft Graph reads from the Purview-imported folders (graph-messages.jsonl, graph_read_import.ps1)
with what classic Outlook read from the same PST files (../outlook/read/*.outlook.jsonl), message by
message, matched by Message-ID within each file (subject when there is none). Fields: subject, sender, recipients
(name and address per to/cc/bcc), read flag, importance, attachments (name, inline, and bytes by SHA-256 where Graph
gave them), plain-text body (SHA-256 of UTF-8, also with CRLF and trailing whitespace normalised).

    python3 -I compare_graph.py > graph-compare.txt
"""
import collections, glob, hashlib, json, os, sys
HERE = os.path.dirname(os.path.abspath(__file__))
READ = os.path.join(HERE, '..', 'outlook', 'read')
IMP = {0: 'low', 1: 'normal', 2: 'high'}
def h(s): return hashlib.sha256(s.encode('utf-8')).hexdigest()
def norm(s): return '\n'.join(line.rstrip() for line in s.replace('\r\n', '\n').replace('\r', '\n').split('\n')).strip('\n')
graph = [json.loads(l) for l in open(os.path.join(HERE, 'graph-messages.jsonl'), encoding='utf-8-sig') if l.strip()]
by_file = collections.defaultdict(list)
for g in graph:
    by_file[g['folder'].split('/')[0].replace('E5-315L-', '').replace('E5-315-', '')].append(g)
totals = collections.Counter(); diffs = collections.defaultdict(list)
for path in sorted(glob.glob(os.path.join(READ, '*.outlook.jsonl'))):
    name = os.path.basename(path).replace('.outlook.jsonl', '')
    src = [json.loads(l) for l in open(path, encoding='utf-8-sig') if l.strip()]
    src = [r for r in src if r.get('kind') == 'message']
    got = by_file.get(name, [])
    # Pair by (Message-ID, subject) first: synthetic suites reuse one Message-ID. Then by Message-ID alone (Exchange may
    # rewrite a subject), then by subject alone.
    remaining = list(got)
    def take(pred):
        for i, g in enumerate(remaining):
            if pred(g): return remaining.pop(i)
    for o in src:
        mid, subj = o.get('message_id'), o['subject']
        g = (take(lambda g: g['internetMessageId'] == mid and g['subject'] == subj)
             or (take(lambda g: g['internetMessageId'] == mid) if mid else None)
             or take(lambda g: g['subject'] == subj))
        if g is None:
            diffs[name].append(('missing in mailbox', subj, mid)); totals['missing'] += 1; continue
        totals['paired'] += 1
        def d(field, a, b):
            if a != b: diffs[name].append((field, o['subject'], a, b)); totals['diff:' + field] += 1
            else: totals['same:' + field] += 1
        d('subject', o['subject'], g['subject'])
        d('sender', (o.get('sender_name'), (o.get('sender_email') or '').lower()), ((g.get('from') or {}).get('name'), ((g.get('from') or {}).get('address') or '').lower()))
        for t, f in ((1, 'to'), (2, 'cc'), (3, 'bcc')):
            d('recipients.' + f, sorted(((r['name'], (r['email'] or '').lower()) for r in o.get('recipients', []) if r['type'] == t)),
              sorted(((r.get('name'), (r.get('address') or '').lower()) for r in g.get(f) or [])))
        d('is_read', o.get('is_read'), g.get('isRead'))
        d('importance', IMP.get(o.get('importance')), g.get('importance'))
        oa = sorted((a.get('long_name') or a.get('name') or '', a.get('sha256')) for a in o.get('attachments', []) if a.get('method') != 5)
        ga = sorted((a.get('name') or '', a.get('sha256')) for a in g.get('attachments', []) if a.get('type') == '#microsoft.graph.fileAttachment')
        d('file_attachments.names', [n for n, _ in oa], [n for n, _ in ga])
        hashed = [(n, s) for n, s in ga if s]
        d('file_attachments.bytes', sorted(x for x in oa if (x[0], x[1]) in hashed or x[0] in {n for n, _ in hashed}), sorted(hashed))
        d('item_attachments.count', sum(1 for a in o.get('attachments', []) if a.get('method') == 5), sum(1 for a in g.get('attachments', []) if a.get('type') == '#microsoft.graph.itemAttachment'))
        if o.get('body_sha256'):
            body = g.get('body') or ''
            exact = h(body) == o['body_sha256']
            totals['body.exact' if exact else 'body.not_exact'] += 1
            if not exact: diffs[name].append(('body (exact sha differs; Exchange text conversion)', o['subject'], o.get('body_chars'), len(body)))
    for g in remaining: diffs[name].append(('extra in mailbox', g['subject'], g['internetMessageId'])); totals['extra'] += 1
print('totals:', json.dumps(dict(sorted(totals.items())), indent=1))
for name in sorted(diffs):
    kinds = collections.Counter(x[0] for x in diffs[name])
    print(f'\n== {name}: {dict(kinds)}')
    for x in [x for x in diffs[name] if not x[0].startswith('body')][:12]: print('  ', json.dumps(x, ensure_ascii=False)[:400])
