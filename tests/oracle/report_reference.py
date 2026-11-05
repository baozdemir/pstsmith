"""Reference derivation of report message properties from a report's MIME (MS-OXCMAIL 2.2.3.7), in Python.

    python3 -B report_reference.py            # the self-check below
    from report_reference import derive        # derive(mime_bytes) -> {"suffix", "properties", "recipients"}

MS-OXCMAIL 2.2.3.7 says how a MIME reader turns a delivery status notification (message/delivery-status, RFC 3464) and a
message disposition notification (message/disposition-notification, RFC 3798) into a report Message object. This is
an independent statement of those rules, in the manifest's property form ({"tag", "type", "value"}), for checking
pstsmith's own implementation (crates/pstsmith/src/convert/mail/report.rs) on the same input. The values are what
MS-OXCMAIL's reader stores, not necessarily what Exchange stores: the writer's and the reader's code tables
(2.1.3.6.1.2 and 2.2.3.7.1.3) are not inverses.

Standard library only (`email`).
"""
import base64
import email
import email.policy
import email.utils
import sys

FILETIME_EPOCH_OFFSET = 11_644_473_600
# 2.2.3.7.1.1: the Action field's severity, and the class suffix of the highest.
SEVERITY = {"delivered": 0, "expanded": 1, "relayed": 2, "delayed": 3, "failed": 4}
DSN_SUFFIX = {"delivered": ".DR", "expanded": ".Expanded.DR", "relayed": ".Relayed.DR", "delayed": ".Delayed.DR", "failed": ".NDR"}
# 2.2.3.7.2: the disposition-type and the class suffix.
MDN_SUFFIX = {"displayed": ".IPNRN", "dispatched": ".IPNRN", "processed": ".IPNRN", "deleted": ".IPNNRN", "denied": ".IPNNRN", "failed": ".IPNNRN"}
# 2.2.3.7.1.3: (Subject, Detail) -> (DiagnosticCode, ReasonCode), None keeping the value as it is; (Subject, None) is
# "any other value" of Detail. A Subject not in the table changes nothing.
CODES = {
    (1, 1): (35, 1), (1, 2): (48, None), (1, 3): (32, None), (1, 4): (1, None), (1, 6): (40, None), (1, None): (0, None),
    (2, 1): (38, None), (2, 2): (13, None), (2, 3): (13, None), (2, 4): (30, None), (2, None): (38, None),
    (3, 1): (38, None), (3, 2): (None, None), (3, 3): (18, None), (3, 4): (13, None), (3, 5): (18, None), (3, None): (38, None),
    (4, 0): (None, None), (4, 3): (None, 6), (4, 4): (None, None), (4, 6): (3, None), (4, 7): (5, None), (4, 8): (3, None), (4, None): (2, None),
    (5, 3): (16, None), (5, 4): (11, None), (5, None): (17, None),
    (6, 2): (9, None), (6, 3): (8, None), (6, 4): (25, None), (6, 5): (None, 2), (6, None): (15, None),
    (7, 1): (29, None), (7, 2): (28, None), (7, 3): (26, None), (7, None): (46, None),
}


def prop(tag, kind, value):
    return {"tag": f"0x{tag:04X}", "type": kind, "value": value}


def filetime(date_header):
    """The report's sent time (its Date header) in FILETIME units."""
    return (int(email.utils.parsedate_to_datetime(date_header).timestamp()) + FILETIME_EPOCH_OFFSET) * 10_000_000


def after_type(value):
    """The part of an RFC 3464 typed field after its type and ';' ("rfc822; a@b" -> "a@b", "dns; mx" -> "mx")."""
    return value.split(";", 1)[1].strip() if ";" in value else value.strip()


def status_codes(status):
    """2.2.3.7.1.3: (PidTagNonDeliveryReportStatusCode, DiagCode, ReasonCode) from a Status field such as "5.1.1"."""
    kind, subject, detail = (int(part) for part in status.strip().split(" ")[0].split(".")[:3])
    subject = subject if 0 <= subject <= 9 else 0
    detail = detail if 0 <= detail <= 9 else 0
    diagnostic, reason = -1, 0
    if any(key[0] == subject for key in CODES):
        new_diagnostic, new_reason = CODES.get((subject, detail), CODES.get((subject, None), (None, None)))
        diagnostic = diagnostic if new_diagnostic is None else new_diagnostic
        reason = reason if new_reason is None else new_reason
    return kind * 100 + subject * 10 + detail, diagnostic, reason


def blocks(part):
    """The field blocks of a message/delivery-status or message/disposition-notification body part."""
    payload = part.get_payload()
    if isinstance(payload, list):
        return [dict(block.items()) for block in payload]
    text = payload if isinstance(payload, str) else part.get_payload(decode=True).decode("ascii", "replace")
    out, current = [], {}
    for line in text.replace("\r\n", "\n").split("\n"):
        if not line.strip():
            if current:
                out.append(current)
                current = {}
        elif line[0] in " \t" and current:
            last = list(current)[-1]
            current[last] += " " + line.strip()
        elif ":" in line:
            name, value = line.split(":", 1)
            current[name.strip()] = value.strip()
    if current:
        out.append(current)
    return out


def field(block, name):
    return next((value for key, value in block.items() if key.lower() == name.lower()), None)


def derive(mime):
    message = email.message_from_bytes(mime, policy=email.policy.compat32)
    sent = filetime(message["Date"])
    for part in message.walk():
        content_type = part.get_content_type()
        if content_type == "message/delivery-status":
            fields = blocks(part)
            per_message, per_recipient = fields[0], fields[1:]
            actions = [(field(block, "Action") or "").lower() for block in per_recipient]
            highest = max(actions, key=lambda action: SEVERITY.get(action, -1)) if actions else None
            properties = []
            if field(per_message, "Reporting-MTA") is not None:
                properties.append(prop(0x6820, "string", field(per_message, "Reporting-MTA")))
            recipients = []
            for block in per_recipient:
                action = (field(block, "Action") or "").lower()
                if action != highest:
                    continue  # 2.2.3.7.1 step 1: only the highest severity's recipients (SHOULD)
                recipient = []
                remote = field(block, "Remote-MTA")
                remote_name = after_type(remote) if remote else None
                if remote_name is not None:
                    recipient.append(prop(0x0C21, "string", remote_name))
                status = field(block, "Status") or ""
                supplementary = field(block, "X-Supplementary-Info")
                if supplementary is None:
                    diagnostic = field(block, "Diagnostic-Code")
                    supplementary = "<" + (remote_name or "") + " #" + status.split(" ")[0] + (" " + diagnostic if diagnostic else "") + ">"
                recipient.append(prop(0x0C1B, "string", supplementary))
                recipient.append(prop(0x0032, "time", sent))
                if action != "failed":
                    recipient.append(prop(0x0010, "time", sent))
                else:
                    code, diagnostic, reason = status_codes(status)
                    recipient += [prop(0x0C20, "int32", code), prop(0x0C05, "int32", diagnostic), prop(0x0C04, "int32", reason)]
                recipients.append({"email": after_type(field(block, "Final-Recipient") or ""), "properties": sorted(recipient, key=lambda p: p["tag"])})
            return {"suffix": DSN_SUFFIX.get(highest), "properties": properties, "recipients": recipients}
        if content_type == "message/disposition-notification":
            notification = blocks(part)[0]
            recipient = field(notification, "Original-Recipient") or field(notification, "Final-Recipient") or ""
            name, address = email.utils.parseaddr(after_type(recipient))
            disposition = field(notification, "Disposition") or ""
            properties = [prop(0x0074, "string", name or address), prop(0x0055, "time", sent), prop(0x002A, "time", sent), prop(0x0032, "time", sent),
                          prop(0x1001, "string", disposition)]
            key = field(notification, "X-MSExch-Correlation-Key")
            if key:
                try:
                    properties.append(prop(0x0025, "binary", base64.b64decode(key, validate=True).hex()))
                except ValueError:
                    pass
            original = field(notification, "Original-Message-ID")
            if original:
                properties += [prop(0x1046, "string", original), prop(0x1039, "string", original)]
            kind = disposition.split(";", 1)[-1].strip().split("/")[0].split()[0].lower() if ";" in disposition else disposition.split("/")[0].strip().lower()
            return {"suffix": MDN_SUFFIX.get(kind), "properties": sorted(properties, key=lambda p: p["tag"]), "recipients": []}
    return {"suffix": None, "properties": [], "recipients": []}


def _check():
    # 2.2.3.7.1.3's own example: "5.7.0" gives Kind 5, Subject 7, Detail 0; Subject 7 with another Detail gives 46.
    assert status_codes("5.7.0") == (570, 46, 0)
    assert status_codes("5.1.1") == (511, 35, 1)
    assert status_codes("4.4.3") == (443, -1, 6)
    assert status_codes("5.6.5") == (565, -1, 2)
    assert status_codes("5.0.0") == (500, -1, 0)
    assert status_codes("5.1.12") == (510, 0, 0)  # Detail above 9 becomes 0, and Subject 1 with Detail 0 is "any other"
    dsn = (b"Date: Mon, 01 Jan 2024 00:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/report; report-type=delivery-status; boundary=b\r\n\r\n"
           b"--b\r\nContent-Type: text/plain\r\n\r\nUndeliverable\r\n--b\r\nContent-Type: message/delivery-status\r\n\r\n"
           b"Reporting-MTA: dns; mx.example.invalid\r\n\r\nFinal-Recipient: rfc822; a@example.invalid\r\nAction: failed\r\nStatus: 5.1.1\r\n"
           b"Remote-MTA: dns; remote.example.invalid\r\nDiagnostic-Code: smtp; 550 5.1.1 unknown\r\n\r\n"
           b"Final-Recipient: rfc822; b@example.invalid\r\nAction: delivered\r\nStatus: 2.0.0\r\n\r\n--b--\r\n")
    result = derive(dsn)
    assert result["suffix"] == ".NDR", result
    assert [r["email"] for r in result["recipients"]] == ["a@example.invalid"], result  # the delivered one is skipped
    values = {p["tag"]: p["value"] for p in result["recipients"][0]["properties"]}
    assert values["0x0C20"] == 511 and values["0x0C05"] == 35 and values["0x0C04"] == 1 and values["0x0C21"] == "remote.example.invalid"
    assert values["0x0C1B"] == "<remote.example.invalid #5.1.1 smtp; 550 5.1.1 unknown>"
    sent = (1704067200 + FILETIME_EPOCH_OFFSET) * 10_000_000
    # 2.2.3.7.1: the report time is the message's sent time (its Date header, 2.2.3.1); a failed recipient has no deliver
    # time; Reporting-MTA is written as its whole value, Remote-MTA as its mta-name portion.
    assert values["0x0032"] == sent and "0x0010" not in values
    assert result["properties"] == [prop(0x6820, "string", "dns; mx.example.invalid")], result
    # A recipient that was not "failed" has its deliver time, the sent time; the highest severity names the class.
    for action, suffix in (("delivered", ".DR"), ("relayed", ".Relayed.DR"), ("expanded", ".Expanded.DR"), ("delayed", ".Delayed.DR")):
        report = derive(dsn.replace(b"Action: failed\r\nStatus: 5.1.1", b"Action: " + action.encode() + b"\r\nStatus: 2.0.0"))
        assert report["suffix"] == suffix, (action, report)
        values = {p["tag"]: p["value"] for p in report["recipients"][0]["properties"]}
        assert values["0x0010"] == sent and values["0x0032"] == sent and "0x0C20" not in values, (action, values)
    # 2.2.3.7.2: a disposition notification sets the original display-to, the three times, the report text, the parent
    # key from the correlation key, and the original Message-ID twice; the disposition-type names the class.
    mdn = (b"Date: Mon, 01 Jan 2024 00:00:00 +0000\r\nMIME-Version: 1.0\r\n"
           b"Content-Type: multipart/report; report-type=disposition-notification; boundary=m\r\n\r\n"
           b"--m\r\nContent-Type: text/plain\r\n\r\nRead\r\n--m\r\nContent-Type: message/disposition-notification\r\n\r\n"
           b"Final-Recipient: rfc822; reader@example.invalid\r\nOriginal-Recipient: rfc822; Reader Name <reader@example.invalid>\r\n"
           b"Original-Message-ID: <original@example.invalid>\r\nX-MSExch-Correlation-Key: AQID\r\n"
           b"Disposition: manual-action/MDN-sent-manually; displayed\r\n\r\n--m--\r\n")
    report = derive(mdn)
    values = {p["tag"]: p["value"] for p in report["properties"]}
    assert report["suffix"] == ".IPNRN" and report["recipients"] == [], report
    assert values == {"0x0025": "010203", "0x002A": sent, "0x0032": sent, "0x0055": sent, "0x0074": "Reader Name",
                      "0x1001": "manual-action/MDN-sent-manually; displayed", "0x1039": "<original@example.invalid>",
                      "0x1046": "<original@example.invalid>"}, values
    assert derive(mdn.replace(b"; displayed", b"; deleted"))["suffix"] == ".IPNNRN"
    # Without a display name, the address; without Original-Recipient, Final-Recipient.
    plain = derive(mdn.replace(b"Original-Recipient: rfc822; Reader Name <reader@example.invalid>\r\n", b""))
    assert {p["tag"]: p["value"] for p in plain["properties"]}["0x0074"] == "reader@example.invalid", plain
    print("report_reference self-check passed")


if __name__ == "__main__":
    _check()
    sys.exit(0)
