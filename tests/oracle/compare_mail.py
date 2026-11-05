"""Independent oracle for the mail input: each fixture converted with pstsmith, read back with libpff, and compared field
by field with what Python's standard `email` package reads from the same source files.

    python3 tests/oracle/compare_mail.py PSTSMITH_BINARY [--negative-control]

Two readers that share no code with pstsmith: libpff (the `pypff` module, from `pip install libpff-python`, or a local
build on PYTHONPATH) reads the PST, and Python's `email` parses the MIME. The source side follows the mapping rules of
`crates/pstsmith/src/convert/mail/mime.rs` (written down again here, not imported): body = the non-attachment
text/plain parts, HTML = the non-attachment text/html parts, every other leaf an attachment with its decoded bytes,
message/rfc822 parts attached messages. Delivery reports are checked against `report_reference.py`, the reference
statement of MS-OXCMAIL 2.2.3.7 in Python. `--negative-control` compares one case's source with another case's PST and
must find mismatches. Exit 0 only when every compared field matches.
"""
import email
import email.policy
import re
import email.utils
import hashlib
import os
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

import pypff

sys.path.insert(0, str(Path(__file__).resolve().parent))
import report_reference  # noqa: E402

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures"
CASES = [
    ("mail-eml", "mail/eml"),
    ("mail-takeout", "mail/takeout/All mail Including Spam and Trash.mbox"),
    ("mail-thunderbird", "mail/thunderbird/Local Folders"),
    ("mail-apple", "mail/apple/Export"),
    ("mail-crlf", "mail/crlf/Windows.mbox"),
    ("mail-maildir", "mail/maildir/Maildir"),
]
GMAIL_FOLDERS = [("Inbox", "Inbox"), ("Sent", "Sent Items"), ("Drafts", "Drafts"), ("Spam", "Junk Email"), ("Trash", "Deleted Items")]


# ---------- the source side: discovery of messages, independent of pstsmith ----------

FROM_LINE = re.compile(rb"^From \S+ .*\d:\d\d.*\b\d{4}\s*$")


def mbox_messages(path):
    """Raw bytes per entry. A "From " line starts an entry when it is the first line (after a byte order mark and blank
    lines) or looks like an MBOX From line (sender, time, four-digit year); mboxrd: one '>' removed from a quoted
    From line; the separating blank line dropped."""
    data = path.read_bytes()
    if data.startswith(b"\xef\xbb\xbf"):
        data = data[3:]
    data = data.lstrip(b"\r\n")
    lines = data.splitlines(keepends=True)
    out, current = [], None
    for line in lines:
        if line.startswith(b"From ") and (current is None or FROM_LINE.match(line.rstrip(b"\r\n"))):
            if current is not None:
                out.append(current)
            current = [b""]
            continue
        if current is None:
            continue
        stripped = line.lstrip(b">")
        if line.startswith(b">") and stripped.startswith(b"From "):
            line = line[1:]
        current[0] += line
    if current is not None:
        out.append(current)
    result = []
    for (raw,) in out:
        if raw.endswith(b"\r\n\r\n"):
            raw = raw[:-2]
        elif raw.endswith(b"\n\n"):
            raw = raw[:-1]
        if b"\nX-Mozilla-Status: " in b"\n" + raw.split(b"\n\n")[0].split(b"\r\n\r\n")[0]:
            header = email.message_from_bytes(raw, policy=email.policy.compat32)
            if int(header.get("X-Mozilla-Status", "0").strip(), 16) & 0x0008:
                continue
        result.append(raw)
    return result


def is_maildir(path):
    return (path / "cur").is_dir() and (path / "new").is_dir()


def maildir_messages(path, folder, prefix, out):
    for sub in ("cur", "new"):
        for entry in sorted((path / sub).iterdir()):
            if entry.name.startswith(".") or not entry.is_file():
                continue
            info = next((entry.name.split(sep, 1)[1] for sep in (":2,", "!2,") if sep in entry.name), "")
            out.append((folder, entry.read_bytes(), {"seen": sub == "cur" and "S" in info, "draft": "D" in info}))
    for entry in sorted(path.iterdir()):
        if entry.name.startswith(".") and len(entry.name) > 1 and is_maildir(entry):
            sub_path = prefix + [part for part in entry.name[1:].split(".") if part]
            maildir_messages(entry, sub_path, sub_path, out)


def source_messages(input_path):
    """[(folder path, raw bytes, flags)] in pstsmith's documented order."""
    out = []
    name = input_path.name
    if input_path.is_dir():
        if is_maildir(input_path):
            maildir_messages(input_path, ["Inbox"], [], out)
        else:
            walk(input_path, [], name, out)
    elif input_path.read_bytes().startswith(b"From "):
        stem = name[:-5] if name.lower().endswith(".mbox") else name
        out += [([stem], raw, {}) for raw in mbox_messages(input_path)]
    else:
        out.append(([input_path.resolve().parent.name], input_path.read_bytes(), {}))
    return out


def walk(directory, folder, top, out):
    for entry in sorted(directory.iterdir()):
        if entry.name.startswith("."):
            continue
        if entry.is_dir():
            stem = entry.name[:-4] if entry.name.endswith(".sbd") else entry.name[:-5] if entry.name.endswith(".mbox") else entry.name
            if is_maildir(entry):
                maildir_messages(entry, folder + [stem], folder + [stem], out)
            else:
                walk(entry, folder + [stem], top, out)
        elif entry.is_file():
            here = folder or [top]
            if entry.name.lower().endswith(".eml"):
                out.append((here, entry.read_bytes(), {}))
            elif entry.read_bytes()[:5] == b"From ":
                target = here if entry.name == "mbox" else folder + [entry.name[:-5] if entry.name.lower().endswith(".mbox") else entry.name]
                out += [(target, raw, {}) for raw in mbox_messages(entry)]


# ---------- what the mapping rules say a message becomes ----------

def is_attachment(part):
    """An attachment disposition, or a file name whatever the disposition."""
    return part.get_content_disposition() == "attachment" or bool((part.get_filename() or "").strip())


def leaves(message):
    # Python's email gives message/* parts (an attached e-mail, a delivery status) sub-parts of their own; for the
    # mapping they are leaves.
    if message.is_multipart() and message.get_content_maintype() != "message":
        for part in message.get_payload():
            yield from leaves(part)
    else:
        yield message


def expected(raw, folder, flags):
    message = email.message_from_bytes(raw, policy=email.policy.default)
    if not any(name for name in message.keys() if name and all(33 <= ord(c) <= 126 and c != ":" for c in name)):
        return {"folder": folder, "subject": "", "class": "IPM.Note", "body": "", "html": None,
                "files": [("message.eml", hashlib.sha256(raw).hexdigest())], "messages": 0, "recipients": [], "read": True}
    labels = str(message.get("X-Gmail-Labels", "") or "")
    label_list = [label.strip().strip('"') for label in labels.split(",") if label.strip()] if labels else []
    if labels:
        folder = [next((outlook for gmail, outlook in GMAIL_FOLDERS if gmail in label_list), "Archive")]
    content_type = message.get_content_type()
    cls, files, messages, body, html = "IPM.Note", [], 0, [], []
    if content_type == "multipart/signed" and "pkcs7-signature" in (message.get_param("protocol") or ""):
        cls = "IPM.Note.SMIME.MultipartSigned"
        files.append(("smime.p7m", None))
        signed = message.get_payload()[0]
        for part in leaves(signed):
            if not is_attachment(part) and part.get_content_type() == "text/plain":
                body.append(part.get_content())
            elif not is_attachment(part) and part.get_content_type() == "text/html":
                html.append(part.get_content())
    elif content_type in ("application/pkcs7-mime", "application/x-pkcs7-mime"):
        cls = "IPM.Note.SMIME"
        files.append(("smime.p7m", hashlib.sha256(message.get_payload(decode=True)).hexdigest()))
    else:
        for part in leaves(message):
            ctype = part.get_content_type()
            if ctype == "message/rfc822":
                messages += 1
            elif is_attachment(part):
                files.append((part.get_filename(), hashlib.sha256(part.get_payload(decode=True) or b"").hexdigest()))
            elif ctype == "text/plain":
                body.append(part.get_content())
            elif ctype == "text/html":
                html.append(part.get_content())
            elif part.get_content_maintype() == "message":
                # A report's status part: its content is checked through the derived report properties.
                files.append((part.get_filename(), None))
            else:
                files.append((part.get_filename(), hashlib.sha256(part.get_payload(decode=True) or b"").hexdigest()))
    recipients = sorted(address.lower() for _, address in email.utils.getaddresses(
        [str(value) for name in ("To", "Cc", "Bcc") for value in message.get_all(name, [])]) if address)
    if content_type == "multipart/report" and message.get("Date"):
        derived = report_reference.derive(raw)
        if derived["suffix"]:
            cls = "REPORT.IPM.Note" + derived["suffix"]
            if any(part.get_content_type() == "message/delivery-status" for part in leaves(message)):
                recipients = sorted(r["email"].lower() for r in derived["recipients"])
    if "seen" in flags:
        read = flags["seen"]
    elif "Unread" in label_list:
        read = False
    elif message.get("X-Mozilla-Status"):
        read = bool(int(str(message["X-Mozilla-Status"]).strip(), 16) & 1)
    elif message.get("Status") or message.get("X-Status"):
        read = "R" in str(message.get("Status", "")) + str(message.get("X-Status", ""))
    else:
        read = True
    return {"folder": folder, "subject": str(message.get("Subject", "") or ""), "class": cls,
            "body": "\n".join(body), "html": "".join(html) if html else None, "files": files, "messages": messages,
            "recipients": recipients, "read": read}


# ---------- the PST side, through libpff ----------

def entries(item, record_set=0):
    out = {}
    rs = item.get_record_set(record_set)
    for i in range(rs.number_of_entries):
        entry = rs.get_entry(i)
        out[entry.entry_type] = (entry.value_type, entry.data)
    return out


def text(props, tag):
    value = props.get(tag)
    if value is None:
        return None
    return (value[1] or b"").decode("utf-16le", "surrogatepass")


def integer(props, tag):
    value = props.get(tag)
    return struct.unpack_from("<i", value[1])[0] if value and value[1] and len(value[1]) >= 4 else None


def pst_messages(path):
    pst = pypff.file()
    pst.open(str(path))
    root = pst.get_root_folder()
    top = next(root.get_sub_folder(i) for i in range(root.number_of_sub_folders) if root.get_sub_folder(i).name == "Top of Outlook data file")
    out = []

    def folder(item, path):
        for i in range(item.number_of_sub_messages):
            message = item.get_sub_message(i)
            props = entries(message)
            recipients = message.recipients
            emails = sorted((text(entries(recipients, k), 0x3003) or "").lower() for k in range(recipients.number_of_record_sets if recipients is not None else 0))
            files, messages = [], 0
            for k in range(message.number_of_attachments):
                attachment = message.get_attachment(k)
                aprops = entries(attachment)
                if integer(aprops, 0x3705) == 5:
                    messages += 1
                    continue
                size = attachment.get_size()
                data = attachment.read_buffer(size) if size else b""
                files.append((text(aprops, 0x3707), hashlib.sha256(data).hexdigest()))
            html = props.get(0x1013)
            out.append({"folder": path, "subject": text(props, 0x0037) or "", "class": text(props, 0x001A),
                        "body": text(props, 0x1000) or "", "html": (html[1] or b"").decode("utf-8", "replace") if html else None,
                        "files": files, "messages": messages, "recipients": emails, "read": bool((integer(props, 0x0E07) or 0) & 1)})
        for i in range(item.number_of_sub_folders):
            child = item.get_sub_folder(i)
            folder(child, path + [child.name])

    folder(top, [])
    return out


# ---------- comparison ----------

def normalized(value):
    return value.replace("\r\n", "\n").rstrip("\n") if isinstance(value, str) else value


def compare(case, sources, pst):
    problems = []
    got = list(pst)
    for folder, raw, flags in sources:
        want = expected(raw, folder, flags)
        match = next((m for m in got if m["folder"] == want["folder"] and m["subject"] == want["subject"]), None)
        if match is None:
            problems.append(f"{case}: no message {want['subject']!r} in {want['folder']}")
            continue
        got.remove(match)
        for field in ("class", "body", "html", "messages", "recipients", "read"):
            if normalized(match[field]) != normalized(want[field]):
                problems.append(f"{case}: {want['subject']!r}: {field}: PST {str(match[field])[:80]!r} != source {str(want[field])[:80]!r}")
        if len(match["files"]) != len(want["files"]):
            problems.append(f"{case}: {want['subject']!r}: {len(match['files'])} file attachments in the PST, {len(want['files'])} in the source")
        got_digests = sorted(digest for _, digest in match["files"])
        want_digests = sorted(digest for _, digest in want["files"] if digest is not None)
        remaining = list(got_digests)
        for digest in want_digests:
            if digest in remaining:
                remaining.remove(digest)
            else:
                problems.append(f"{case}: {want['subject']!r}: an attachment's bytes (sha256 {digest[:12]}) are not in the PST")
        got_names = [name for name, _ in match["files"]]
        for name, _ in want["files"]:
            if name is not None and name not in got_names:
                problems.append(f"{case}: {want['subject']!r}: attachment name {name!r} not in the PST ({got_names})")
    for extra in got:
        problems.append(f"{case}: unexpected message {extra['subject']!r} in {extra['folder']}")
    return problems


def main():
    binary = sys.argv[1]
    negative = "--negative-control" in sys.argv[2:]
    problems, compared = [], 0
    with tempfile.TemporaryDirectory() as out:
        outputs = {}
        for case, input_path in CASES:
            pst = Path(out) / f"{case}.pst"
            subprocess.run([binary, "convert", str(FIXTURES / input_path), "-o", str(pst)], check=True, capture_output=True)
            outputs[case] = pst
        for index, (case, input_path) in enumerate(CASES):
            other = CASES[(index + 1) % len(CASES)][0] if negative else case
            sources = source_messages(FIXTURES / input_path)
            compared += len(sources)
            problems += compare(case, sources, pst_messages(outputs[other]))
    for problem in problems:
        print(problem)
    print(f"{compared} messages compared, {len(problems)} mismatches")
    if negative:
        sys.exit(0 if problems else 1)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
