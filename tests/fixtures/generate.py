"""Writes pstsmith's test fixtures. Python 3 standard library only; deterministic.

    python3 tests/fixtures/generate.py           # the 64 MiB attachment the manifest fixtures name (not committed)
    python3 tests/fixtures/generate.py --mail    # also rewrite the committed mail fixtures under tests/fixtures/mail

Every message is synthetic: every address is in example.invalid, no text comes from real mail.
"""
import base64
import hashlib
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FILES = HERE / "manifest" / "files"
MAIL = HERE / "mail"


def large_attachment():
    """large-64MiB.bin: the first MiB of SHAKE-256("large-64MiB.bin"), repeated 64 times (the manifests pin its SHA-256)."""
    path = FILES / "large-64MiB.bin"
    block = hashlib.shake_256(b"large-64MiB.bin").digest(1024 * 1024)
    if path.exists() and path.stat().st_size == 64 * len(block):
        return
    with path.open("wb") as out:
        for _ in range(64):
            out.write(block)


def crlf(text):
    return text.replace("\r\n", "\n").replace("\n", "\r\n").encode("utf-8")


def eml(subject, body="Synthetic body.\n", extra="", sender="Sentetik Gönderen <sender@example.invalid>", date="Mon, 01 Jan 2024 10:00:00 +0000", msgid=None):
    msgid = msgid or subject.lower().replace(" ", "-").encode("ascii", "replace").decode() + "@example.invalid"
    return (f"From: {sender}\nTo: To User <to@example.invalid>\nSubject: {subject}\nDate: {date}\nMessage-ID: <{msgid}>\n{extra}"
            f"MIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\n{body}")


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else crlf(data))


def mail():
    if MAIL.exists():
        shutil.rmtree(MAIL)
    e = MAIL / "eml"
    write(e / "plain-utf8.eml", eml("Plain message", "Türkçe ğüşöçı İ, 日本語, emoji 🙂.\nSecond line.\n"))
    write(e / "encoded-words.eml", (
        b"From: =?windows-1254?Q?=DEule_Y=FDlmaz?= <sule@example.invalid>\r\nTo: =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?= <jp@example.invalid>\r\n"
        b"Subject: =?windows-1254?Q?=DEubat_raporu_=FC=E7=FC=FE?= =?UTF-8?B?4pyT?=\r\nDate: Tue, 02 Jan 2024 09:30:00 +0300\r\n"
        b"Message-ID: <encoded-words@example.invalid>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=windows-1254\r\n"
        b"Content-Transfer-Encoding: 8bit\r\n\r\n\xc7al\xfd\xfemalar tamam.\r\n"))
    # Legacy charsets in a subject (ISO-8859-9, ISO-2022-JP) and in attachment names (RFC 2231 in ISO-8859-9, RFC 2047
    # in windows-1254, as many mail programs still write them).
    write(e / "legacy-charsets.eml", (
        b"From: sender@example.invalid\r\nTo: to@example.invalid\r\n"
        b"Subject: =?ISO-8859-9?Q?=DEirket_duyurusu_=F0=FC=FE=F6=E7=FD_=DD?= =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=\r\n"
        b"Date: Tue, 02 Jan 2024 10:30:00 +0300\r\nMessage-ID: <legacy-charsets@example.invalid>\r\nMIME-Version: 1.0\r\n"
        b"Content-Type: multipart/mixed; boundary=\"lc\"\r\n\r\n--lc\r\nContent-Type: text/plain; charset=iso-8859-9\r\n"
        b"Content-Transfer-Encoding: 8bit\r\n\r\n\xdeirket notu.\r\n"
        b"--lc\r\nContent-Type: text/plain; charset=iso-8859-9\r\n"
        b"Content-Disposition: attachment; filename*=iso-8859-9''%DE%FD%F0%20notlar%FD.txt\r\n\r\nnot\r\n"
        b"--lc\r\nContent-Type: application/octet-stream; name=\"=?windows-1254?Q?=E7=FD=F0=FE=FC_=DD.bin?=\"\r\n"
        b"Content-Disposition: attachment; filename=\"=?windows-1254?Q?=E7=FD=F0=FE=FC_=DD.bin?=\"\r\n"
        b"Content-Transfer-Encoding: base64\r\n\r\nAAECAw==\r\n--lc--\r\n"))
    write(e / "alternative-inline-image.eml", (
        "From: sender@example.invalid\nTo: to@example.invalid\nSubject: Inline image\nDate: Wed, 03 Jan 2024 08:00:00 +0000\n"
        "Message-ID: <inline-image@example.invalid>\nMIME-Version: 1.0\nContent-Type: multipart/related; boundary=\"rel\"\n\n"
        "--rel\nContent-Type: multipart/alternative; boundary=\"alt\"\n\n--alt\nContent-Type: text/plain; charset=utf-8\n\n"
        "See the logo.\n--alt\nContent-Type: text/html; charset=utf-8\n\n<p>See the <img src=\"cid:logo@example.invalid\"> logo.</p>\n"
        "--alt--\n--rel\nContent-Type: image/png\nContent-ID: <logo@example.invalid>\nContent-Transfer-Encoding: base64\n\n"
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==\n--rel--\n"))
    pdf = base64.b64encode(b"%PDF-1.4\n% synthetic\n").decode()
    write(e / "attachments.eml", (
        "From: sender@example.invalid\nTo: to@example.invalid\nSubject: Attachments\nDate: Thu, 04 Jan 2024 08:00:00 +0000\n"
        "Message-ID: <attachments@example.invalid>\nImportance: High\nKeywords: Project X, Müşteri\nMIME-Version: 1.0\n"
        "Content-Type: multipart/mixed; boundary=\"mix\"\n\n--mix\nContent-Type: text/plain; charset=utf-8\n\nSee attached.\n"
        f"--mix\nContent-Type: application/pdf\nContent-Disposition: attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf\n"
        f"Content-Transfer-Encoding: base64\n\n{pdf}\n"
        "--mix\nContent-Type: text/plain; charset=iso-8859-9; name=\"notlar.txt\"\nContent-Disposition: attachment\n"
        "Content-Transfer-Encoding: quoted-printable\n\n=DE=FD=F0 notlar=FD\n"
        "--mix\nContent-Type: application/ms-tnef; name=\"winmail.dat\"\nContent-Transfer-Encoding: base64\n\neJ8+IgABBgAB\n"
        "--mix\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\n\nBEGIN:VCALENDAR\nVERSION:2.0\nEND:VCALENDAR\n--mix--\n"))
    inner = eml("Inner message", "Forwarded twice.\n", sender="inner@example.invalid", msgid="inner@example.invalid")
    middle = ("From: middle@example.invalid\nTo: to@example.invalid\nSubject: Middle message\nDate: Mon, 01 Jan 2024 09:00:00 +0000\n"
              "Message-ID: <middle@example.invalid>\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"m2\"\n\n"
              f"--m2\nContent-Type: text/plain\n\nForwarded once.\n--m2\nContent-Type: message/rfc822\n\n{inner}\n--m2--\n")
    write(e / "forwarded-nested.eml", (
        "From: sender@example.invalid\nTo: to@example.invalid\nSubject: Fw: Middle message\nDate: Fri, 05 Jan 2024 08:00:00 +0000\n"
        "Message-ID: <forwarded@example.invalid>\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"m1\"\n\n"
        f"--m1\nContent-Type: text/plain\n\nForwarding.\n--m1\nContent-Type: message/rfc822\n\n{middle}\n--m1--\n"))
    signed = (FILES / "smime" / "clear-signed.smime.p7m").read_bytes()
    write(e / "smime-signed.eml", crlf("From: Synthetic SMIME Sender <sender@example.invalid>\nTo: to@example.invalid\n"
                                       "Subject: Signed message\nDate: Sat, 06 Jan 2024 08:00:00 +0000\n"
                                       "Message-ID: <smime-signed@example.invalid>\nMIME-Version: 1.0\n") + signed)
    encrypted = base64.encodebytes((FILES / "smime" / "encrypted.smime.p7m").read_bytes()).decode()
    write(e / "smime-encrypted.eml", (
        "From: Synthetic SMIME Sender <sender@example.invalid>\nTo: to@example.invalid\nSubject: Encrypted message\n"
        "Date: Sun, 07 Jan 2024 08:00:00 +0000\nMessage-ID: <smime-encrypted@example.invalid>\nMIME-Version: 1.0\n"
        "Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=smime.p7m\nContent-Transfer-Encoding: base64\n"
        f"Content-Disposition: attachment; filename=smime.p7m\n\n{encrypted}"))
    for report in sorted((HERE / "reports").glob("*.eml")):
        write(e / f"report-{report.name}", report.read_bytes())
    write(e / "broken-boundary.eml", (
        "From: sender@example.invalid\nSubject: Broken boundary\nDate: Mon, 08 Jan 2024 08:00:00 +0000\nMIME-Version: 1.0\n"
        "Content-Type: multipart/mixed; boundary=\"never-closed\"\n\n--never-closed\nContent-Type: text/plain\n\nNo closing boundary.\n"))
    write(e / "no-headers.eml", b"\x00\x01 these bytes are not a message\r\n")

    takeout = ""
    for index, (labels, subject) in enumerate([
        ("Inbox,Important,Category Updates", "Takeout inbox"), ("Sent,Opened", "Takeout sent"),
        ("Archived,Receipts,\"Projects, 2026\"", "Takeout archived"), ("Inbox,Unread,Starred", "Takeout unread"),
        ("Trash", "Takeout trash"), ("Spam", "Takeout spam"), ("Drafts", "Takeout draft")]):
        takeout += f"From 18{index:015d}@xxx Mon Jan 01 10:0{index}:00 +0000 2024\nX-GM-THRID: 1{index:018d}\nX-Gmail-Labels: {labels}\n"
        takeout += eml(subject, date=f"Mon, 01 Jan 2024 10:0{index}:00 +0000") + "\n"
    write(MAIL / "takeout" / "All mail Including Spam and Trash.mbox", takeout.replace("\r\n", "\n").encode())
    tb = MAIL / "thunderbird" / "Local Folders"
    write(tb / "Inbox", ("From - Mon Jan  1 10:00:00 2024\nX-Mozilla-Status: 0001\n" + eml("Read in Thunderbird") + "\n"
                         "From - Mon Jan  1 11:00:00 2024\nX-Mozilla-Status: 0000\n"
                         + eml("Unread in Thunderbird", ">From the start of a line.\n>>From quoted twice.\n") + "\n"
                         "From - Mon Jan  1 12:00:00 2024\nX-Mozilla-Status: 0009\n" + eml("Deleted, not compacted") + "\n").encode())
    write(tb / "Inbox.msf", b"// Mork index, not mail\n")
    # Takeout mail filed in Thunderbird keeps its X-Gmail-Labels; the Thunderbird folder wins.
    write(tb / "Inbox.sbd" / "Work", ("From - Tue Jan  2 10:00:00 2024\nX-Mozilla-Status: 0001\nX-Gmail-Labels: Archived,Clients\n"
                                     + eml("At work") + "\n").encode())
    write(MAIL / "apple" / "Export" / "Clients.mbox" / "mbox", ("From sender@example.invalid Mon Jan  1 10:00:00 2024\n" + eml("Apple Mail") + "\n").encode())
    write(MAIL / "apple" / "Export" / "Clients.mbox" / "Info.plist", b"<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict/></plist>\n")
    # A body line that only looks like a From line stays in its message; a From line whose date is written as in a
    # Date header, after a blank line and before a header field, starts one.
    write(MAIL / "crlf" / "Windows.mbox", crlf("From sender@example.invalid Mon Jan  1 10:00:00 2024\n"
                                                + eml("CRLF one", "Hours:\n\nFrom 9:00 to 17:00\nLocation:Room 5\n") + "\n"
                                                "From sender@example.invalid Mon Jan  1 11:00:00 2024\n" + eml("CRLF two") + "\n"
                                                "From sender@example.invalid Tue, 02 Jan 2024 09:00:00 +0000\n" + eml("CRLF Date-style separator") + "\n"
                                                # UW c-client and Alpine write the zone after the year; System V's %Z
                                                # (a name, tzdata's "+03", two words) before it; UUCP adds "remote from".
                                                "From sender@example.invalid Wed Jan  3 09:00:00 2024 +0100\n" + eml("CRLF zone after the year") + "\n"
                                                "From sender@example.invalid Wed Jan  3 10:00 PST 2024\n" + eml("CRLF System V zone") + "\n"
                                                "From sender@example.invalid Wed Jan  3 11:00:00 +03 2024\n" + eml("CRLF numeric abbreviation") + "\n"
                                                "From sender@example.invalid Wed Jan  3 12:00:00 MET DST 2024\n" + eml("CRLF two zone words") + "\n"
                                                "From sender@example.invalid Wed Jan  3 13:00:00 2024 GMT\n" + eml("CRLF zone name after the year") + "\n"
                                                "From sender Wed Jan  3 14:00:00 2024 remote from host\n" + eml("CRLF UUCP") + "\n"))
    md = MAIL / "maildir" / "Maildir"
    # Maildir++ also writes "!2," where ':' is not allowed in a file name (Windows); both are read.
    write(md / "cur" / "1704103200.1.host!2,S", eml("Maildir seen"))
    write(md / "new" / "1704106800.2.host", eml("Maildir new"))
    (md / "tmp").mkdir(parents=True, exist_ok=True)
    write(md / ".Sent" / "cur" / "1704110400.3.host!2,S", eml("Maildir sent"))
    (md / ".Sent" / "new").mkdir(parents=True, exist_ok=True)
    write(md / ".Archive.2024" / "cur" / "1704114000.4.host!2,DS", eml("Maildir draft"))
    (md / ".Archive.2024" / "new").mkdir(parents=True, exist_ok=True)
    # Dovecot and Courier store folder names in IMAP's modified UTF-7 ("Gönderilmiş", "Ar&Ge"); a store set to UTF-8
    # has the name as it is ("Taslaklar Ğİ").
    write(md / ".G&APY-nderilmi&AV8-" / "cur" / "1704117600.5.host!2,S", eml("Maildir modified UTF-7"))
    write(md / ".Ar&-Ge" / "new" / "1704121200.6.host", eml("Maildir ampersand"))
    write(md / ".Taslaklar Ğİ" / "cur" / "1704124800.7.host!2,DS", eml("Maildir UTF-8 name"))
    (md / ".G&APY-nderilmi&AV8-" / "new").mkdir(parents=True, exist_ok=True)
    (md / ".Ar&-Ge" / "cur").mkdir(parents=True, exist_ok=True)
    (md / ".Taslaklar Ğİ" / "new").mkdir(parents=True, exist_ok=True)
    for empty in (md / "tmp", md / ".Sent" / "new", md / ".Archive.2024" / "new", md / ".G&APY-nderilmi&AV8-" / "new",
                  md / ".Ar&-Ge" / "cur", md / ".Taslaklar Ğİ" / "new"):
        (empty / ".keep").write_bytes(b"")


if __name__ == "__main__":
    large_attachment()
    if "--mail" in sys.argv[1:]:
        mail()
