//! Tests of the MIME mapping, one per rule of the mapping (see `mime.rs`).

use super::mime::{message_from_mime, MAX_NESTING};
use super::test_stage::MemoryStage;
use super::Origin;
use serde_json::{json, Value};

fn origin() -> Origin {
    Origin {
        container: "Inbox.mbox".into(),
        ordinal: 1,
        ..Origin::default()
    }
}

fn map(raw: &str) -> (Value, MemoryStage) {
    let mut stage = MemoryStage::default();
    let item = message_from_mime(raw.as_bytes(), &origin(), &mut stage).unwrap();
    (item.message, stage)
}

fn map_bytes(raw: &[u8], origin: &Origin) -> (super::mime::MailItem, MemoryStage) {
    let mut stage = MemoryStage::default();
    let item = message_from_mime(raw, origin, &mut stage).unwrap();
    (item, stage)
}

fn staged<'a>(stage: &'a MemoryStage, attachment: &Value) -> &'a [u8] {
    let path = attachment["path"].as_str().unwrap();
    &stage.files[path.strip_prefix("mem:").unwrap().parse::<usize>().unwrap()]
}

const SIMPLE: &str = "From: Sender Name <sender@example.invalid>\r\nTo: To One <to1@example.invalid>, to2@example.invalid\r\n\
Cc: Team: a@example.invalid, B <b@example.invalid>;\r\nSubject: =?UTF-8?Q?G=C3=BCnayd=C4=B1n?=\r\n\
Date: Mon, 01 Jan 2024 10:00:00 +0200\r\nMessage-ID: <simple@example.invalid>\r\n\
Received: from relay.example.invalid by mx.example.invalid; Mon, 01 Jan 2024 08:00:05 +0000\r\n\
Received: from client.example.invalid by relay.example.invalid; Mon, 01 Jan 2024 08:00:01 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nHello\r\n";

#[test]
fn subject_decodes_rfc2047() {
    assert_eq!(map(SIMPLE).0["subject"], "Günaydın");
}

#[test]
fn sender_and_message_id() {
    let (message, _) = map(SIMPLE);
    assert_eq!(
        message["sender"],
        json!({"name": "Sender Name", "email": "sender@example.invalid"})
    );
    assert_eq!(message["message_id"], "<simple@example.invalid>");
    assert_eq!(message["message_class"], "IPM.Note");
}

#[test]
fn recipients_flatten_groups() {
    let (message, _) = map(SIMPLE);
    assert_eq!(
        message["to"],
        json!([{"name": "To One", "email": "to1@example.invalid", "properties": []}, {"name": "", "email": "to2@example.invalid", "properties": []}])
    );
    assert_eq!(
        message["cc"],
        json!([{"name": "", "email": "a@example.invalid", "properties": []}, {"name": "B", "email": "b@example.invalid", "properties": []}])
    );
    assert_eq!(message["bcc"], json!([]));
}

#[test]
fn received_time_prefers_topmost_received() {
    let (message, _) = map(SIMPLE);
    assert_eq!(message["sent_time"], 1_704_096_000);
    assert_eq!(message["received_time"], 1_704_096_005);
}

#[test]
fn received_time_falls_back_to_the_mbox_date_then_date() {
    let raw = SIMPLE.replace("Received: from relay.example.invalid by mx.example.invalid; Mon, 01 Jan 2024 08:00:05 +0000\r\n", "")
        .replace("Received: from client.example.invalid by relay.example.invalid; Mon, 01 Jan 2024 08:00:01 +0000\r\n", "");
    let mbox = Origin {
        mbox_date: Some(1_704_100_000),
        ..origin()
    };
    assert_eq!(
        map_bytes(raw.as_bytes(), &mbox).0.message["received_time"],
        1_704_100_000
    );
    assert_eq!(map(&raw).0["received_time"], 1_704_096_000);
}

#[test]
fn times_are_deterministic() {
    let (message, _) = map(SIMPLE);
    assert_eq!(message["created_time"], message["received_time"]);
    assert_eq!(message["last_modified_time"], message["received_time"]);
    assert_eq!(map(SIMPLE).0, message);
}

#[test]
fn transport_headers_are_the_header_block() {
    let (message, _) = map(SIMPLE);
    let headers = message["transport_headers"].as_str().unwrap();
    assert!(headers.starts_with("From: Sender Name <sender@example.invalid>\r\n"));
    assert!(headers.ends_with("Content-Type: text/plain; charset=utf-8\r\n"));
}

#[test]
fn transport_headers_utf8_then_latin1() {
    let raw = b"From: a@example.invalid\r\nX-Note: caf\xe9\r\nSubject: s\r\n\r\nbody\r\n";
    let (item, _) = map_bytes(raw, &origin());
    assert!(item.message["transport_headers"]
        .as_str()
        .unwrap()
        .contains("X-Note: café\r\n"));
}

#[test]
fn plain_body_and_no_html() {
    let (message, _) = map(SIMPLE);
    assert_eq!(message["body"], "Hello\r\n");
    assert_eq!(message["html"], Value::Null);
    assert_eq!(message["attachments"], json!([]));
}

const ALTERNATIVE: &str = "From: a@example.invalid\r\nSubject: alt\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=r\r\n\r\n--r\r\nContent-Type: multipart/alternative; boundary=a\r\n\r\n\
--a\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain text\r\n--a\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<p>HTML <img src=\"cid:logo@example.invalid\"></p>\r\n--a--\r\n--r\r\nContent-Type: image/png\r\n\
Content-ID: <logo@example.invalid>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n\
--r\r\nContent-Type: image/gif; name=other.gif\r\nContent-ID: <unused@example.invalid>\r\nContent-Transfer-Encoding: base64\r\n\r\nR0lGODlh\r\n--r--\r\n";

#[test]
fn alternative_gives_body_and_html() {
    let (message, _) = map(ALTERNATIVE);
    assert_eq!(message["body"], "Plain text");
    assert_eq!(
        message["html"],
        "<p>HTML <img src=\"cid:logo@example.invalid\"></p>"
    );
}

#[test]
fn cid_part_is_inline() {
    let (message, stage) = map(ALTERNATIVE);
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0]["kind"], "file");
    assert_eq!(attachments[0]["inline"], true);
    assert_eq!(attachments[0]["content_id"], "logo@example.invalid");
    assert_eq!(attachments[0]["mime"], "image/png");
    assert_eq!(attachments[0]["name"], "attachment-1.png");
    assert_eq!(staged(&stage, &attachments[0]), b"\x89PNG\r\n\x1a\n");
    assert_eq!(attachments[0]["size"], 8);
    // A Content-ID the HTML does not reference stays a listed attachment.
    assert_eq!(attachments[1]["inline"], false);
    assert_eq!(attachments[1]["content_id"], "unused@example.invalid");
    assert_eq!(attachments[1]["name"], "other.gif");
}

#[test]
fn html_only_has_empty_body() {
    let raw = "From: a@example.invalid\r\nSubject: h\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<b>only html</b>\r\n";
    let (message, _) = map(raw);
    assert_eq!(message["body"], "");
    assert_eq!(message["html"], "<b>only html</b>\r\n");
}

const MIXED: &str = "From: a@example.invalid\r\nSubject: files\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n\
--m\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSee attached.\r\n\
--m\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQ=\r\n\
--m\r\nContent-Type: text/plain; charset=iso-8859-9; name=\"notlar.txt\"\r\nContent-Disposition: attachment\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\r\n=FE=F0=FD\r\n\
--m\r\nContent-Type: application/ms-tnef; name=\"winmail.dat\"\r\nContent-Transfer-Encoding: base64\r\n\r\neJ8+IgA=\r\n\
--m\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
--m--\r\n";

#[test]
fn rfc2231_filename() {
    let (message, stage) = map(MIXED);
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments[0]["name"], "résumé.pdf");
    assert_eq!(attachments[0]["mime"], "application/pdf");
    assert_eq!(staged(&stage, &attachments[0]), b"%PDF-1.4");
}

#[test]
fn text_attachments_keep_their_bytes() {
    let (message, stage) = map(MIXED);
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments[1]["name"], "notlar.txt");
    assert_eq!(
        staged(&stage, &attachments[1]),
        b"\xfe\xf0\xfd",
        "ISO-8859-9 bytes, not re-encoded"
    );
}

#[test]
fn tnef_and_ics_stay_files() {
    let (message, stage) = map(MIXED);
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 4);
    assert_eq!(attachments[2]["name"], "winmail.dat");
    assert_eq!(attachments[2]["mime"], "application/ms-tnef");
    assert_eq!(attachments[3]["name"], "attachment-4.ics");
    assert_eq!(attachments[3]["mime"], "text/calendar");
    assert_eq!(
        staged(&stage, &attachments[3]),
        b"BEGIN:VCALENDAR\r\nEND:VCALENDAR"
    );
    assert_eq!(message["body"], "See attached.");
}

fn nested(depth: usize) -> String {
    let mut inner = "From: deep@example.invalid\r\nSubject: level 0\r\nContent-Type: text/plain\r\n\r\ninnermost\r\n".to_string();
    for level in 1..=depth {
        inner = format!(
            "From: l{level}@example.invalid\r\nSubject: level {level}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b{level}\r\n\r\n\
             --b{level}\r\nContent-Type: text/plain\r\n\r\nwrapper {level}\r\n--b{level}\r\nContent-Type: message/rfc822\r\n\r\n{inner}\r\n--b{level}--\r\n"
        );
    }
    inner
}

#[test]
fn rfc822_part_is_attached_message() {
    let (message, _) = map(&nested(1));
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["kind"], "message");
    assert_eq!(attachments[0]["name"], "level 0");
    assert_eq!(attachments[0]["inline"], false);
    let inner = &attachments[0]["message"];
    assert_eq!(inner["subject"], "level 0");
    assert_eq!(inner["body"], "innermost\r\n");
    assert!(inner.get("item_key").is_none() && inner.get("folder_path").is_none());
    assert_ne!(inner["search_key"], message["search_key"]);
}

fn depth_of(message: &Value) -> usize {
    match message["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "message")
    {
        Some(attachment) => 1 + depth_of(&attachment["message"]),
        None => 0,
    }
}

#[test]
fn nesting_deeper_than_three_levels_is_parsed() {
    assert_eq!(depth_of(&map(&nested(6)).0), 6);
}

#[test]
fn nesting_deeper_than_32_becomes_an_eml_file() {
    let (item, stage) = map_bytes(nested(MAX_NESTING + 2).as_bytes(), &origin());
    assert_eq!(depth_of(&item.message), MAX_NESTING);
    let mut deepest = &item.message;
    while let Some(attachment) = deepest["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "message")
    {
        deepest = &attachment["message"];
    }
    let file = deepest["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "file")
        .unwrap();
    assert_eq!(file["mime"], "message/rfc822");
    assert!(file["name"].as_str().unwrap().ends_with(".eml"));
    assert!(staged(&stage, file).starts_with(b"From: l1@example.invalid"));
    assert_eq!(item.warnings.len(), 1);
}

#[test]
fn importance_header_wins_over_x_priority() {
    let with = |headers: &str| {
        map(&SIMPLE.replace("Subject:", &format!("{headers}Subject:"))).0["importance"].clone()
    };
    assert_eq!(with(""), 1);
    assert_eq!(with("Importance: High\r\nX-Priority: 5\r\n"), 2);
    assert_eq!(with("Importance: low\r\n"), 0);
    assert_eq!(with("X-Priority: 1 (Highest)\r\n"), 2);
    assert_eq!(with("X-Priority: 4\r\n"), 0);
    assert_eq!(with("X-Priority: 3\r\n"), 1);
}

#[test]
fn keywords_become_categories() {
    let (message, _) = map(&SIMPLE.replace(
        "Subject:",
        "Keywords: Project X, Müşteri, Project X\r\nSubject:",
    ));
    assert_eq!(message["categories"], json!(["Project X", "Müşteri"]));
}

#[test]
fn gmail_labels_set_folder_read_state_and_categories() {
    let raw = SIMPLE.replace(
        "Subject:",
        "X-Gmail-Labels: Sent,Unread,Starred,Müşteri\r\nSubject:",
    );
    let (item, _) = map_bytes(raw.as_bytes(), &origin());
    assert_eq!(item.folder.as_deref(), Some("Sent Items"));
    assert_eq!(item.message["is_read"], false);
    assert_eq!(item.message["categories"], json!(["Starred", "Müşteri"]));
}

#[test]
fn read_state_order() {
    let read = |headers: &str, origin: &Origin| {
        let raw = SIMPLE.replace("Subject:", &format!("{headers}Subject:"));
        map_bytes(raw.as_bytes(), origin).0.message["is_read"].clone()
    };
    let plain = origin();
    assert_eq!(read("", &plain), true);
    assert_eq!(read("X-Mozilla-Status: 0000\r\n", &plain), false);
    assert_eq!(read("X-Mozilla-Status: 0001\r\n", &plain), true);
    assert_eq!(read("Status: O\r\n", &plain), false);
    assert_eq!(read("Status: RO\r\n", &plain), true);
    assert_eq!(read("X-Status: R\r\n", &plain), true);
    // An answered message was read (mutt, Eudora: X-Status A).
    assert_eq!(read("X-Status: A\r\n", &plain), true);
    // The Maildir flags are the source's own state and come first.
    let unseen = Origin {
        seen: Some(false),
        ..origin()
    };
    assert_eq!(read("Status: RO\r\n", &unseen), false);
    let seen = Origin {
        seen: Some(true),
        ..origin()
    };
    assert_eq!(read("X-Gmail-Labels: Unread\r\n", &seen), true);
    assert_eq!(
        read("X-Gmail-Labels: Unread\r\nStatus: RO\r\n", &plain),
        false
    );
}

#[test]
fn draft_flags() {
    let maildir = Origin {
        draft: true,
        ..origin()
    };
    let (item, _) = map_bytes(SIMPLE.as_bytes(), &maildir);
    assert_eq!(item.message["is_draft"], true);
    assert_eq!(item.message["sent_time"], Value::Null);
    assert_eq!(item.message["received_time"], Value::Null);
    let gmail = SIMPLE.replace("Subject:", "X-Gmail-Labels: Drafts\r\nSubject:");
    assert_eq!(
        map_bytes(gmail.as_bytes(), &origin()).0.message["is_draft"],
        true
    );
    assert_eq!(map(SIMPLE).0["is_draft"], false);
}

#[test]
fn thunderbird_draft_info_is_not_a_draft() {
    // Thunderbird keeps X-Mozilla-Draft-Info on messages it sent and copied to Sent; it is no draft state.
    let raw = SIMPLE.replace(
        "Subject:",
        "X-Mozilla-Draft-Info: internal/draft; vcard=0\r\nSubject:",
    );
    let (item, _) = map_bytes(raw.as_bytes(), &origin());
    assert_eq!(item.message["is_draft"], false);
    assert_eq!(item.message["sent_time"], 1_704_096_000);
}

const SIGNED: &str = "From: a@example.invalid\r\nSubject: signed\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256;\r\n boundary=s\r\n\r\n\
--s\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSigned text\r\n--s\r\nContent-Type: application/pkcs7-signature; name=smime.p7s\r\n\
Content-Transfer-Encoding: base64\r\n\r\nAAEC\r\n--s--\r\n";

#[test]
fn smime_signed_class_and_p7m() {
    let (message, stage) = map(SIGNED);
    assert_eq!(message["message_class"], "IPM.Note.SMIME.MultipartSigned");
    assert_eq!(message["body"], "Signed text");
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["name"], "smime.p7m");
    assert_eq!(attachments[0]["mime"], "multipart/signed");
    let p7m = staged(&stage, &attachments[0]);
    let text = std::str::from_utf8(p7m).unwrap();
    assert!(text.starts_with("Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256;\r\n boundary=s\r\n\r\n--s\r\n"));
    assert!(text.ends_with("AAEC\r\n--s--\r\n"));
}

#[test]
fn smime_opaque_class_and_p7m() {
    let raw = "From: a@example.invalid\r\nSubject: sealed\r\nMIME-Version: 1.0\r\n\
Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=smime.p7m\r\nContent-Transfer-Encoding: base64\r\n\r\nAAECAw==\r\n";
    let (message, stage) = map(raw);
    assert_eq!(message["message_class"], "IPM.Note.SMIME");
    assert_eq!(message["body"], "");
    let attachments = message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["name"], "smime.p7m");
    assert_eq!(attachments[0]["mime"], "application/pkcs7-mime");
    assert_eq!(staged(&stage, &attachments[0]), b"\x00\x01\x02\x03");
}

const DSN: &str = "From: MAILER-DAEMON@mx.example.invalid\r\nTo: sender@example.invalid\r\nSubject: Undeliverable: hello\r\n\
Date: Mon, 01 Jan 2024 00:00:00 +0000\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/report; report-type=delivery-status; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nUndeliverable\r\n--b\r\nContent-Type: message/delivery-status\r\n\r\n\
Reporting-MTA: dns; mx.example.invalid\r\n\r\nFinal-Recipient: rfc822; a@example.invalid\r\nAction: failed\r\nStatus: 5.1.1\r\n\
Remote-MTA: dns; remote.example.invalid\r\nDiagnostic-Code: smtp; 550 5.1.1 unknown\r\n\r\n--b--\r\n";

#[test]
fn dsn_report_class_and_recipient_properties() {
    let (message, _) = map(DSN);
    assert_eq!(message["message_class"], "REPORT.IPM.Note.NDR");
    assert_eq!(message["subject"], "Undeliverable: hello");
    let to = message["to"].as_array().unwrap();
    assert_eq!(to.len(), 1);
    assert_eq!(to[0]["name"], "a@example.invalid");
    assert_eq!(to[0]["email"], "a@example.invalid");
    let status = to[0]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tag"] == "0x0C20")
        .unwrap();
    assert_eq!(status["value"], 511);
    assert_eq!(message["properties"][0]["tag"], "0x6820");
    assert_eq!(message["body"], "Undeliverable");
    // The status part itself is kept as a file, so nothing of the report is lost.
    assert_eq!(message["attachments"][0]["mime"], "message/delivery-status");
}

#[test]
fn mdn_report_class() {
    let raw = "From: reader@example.invalid\r\nTo: sender@example.invalid\r\nSubject: Read: hello\r\n\
Date: Mon, 01 Jan 2024 00:00:00 +0000\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/report; report-type=disposition-notification; boundary=m\r\n\r\n\
--m\r\nContent-Type: text/plain\r\n\r\nRead\r\n--m\r\nContent-Type: message/disposition-notification\r\n\r\n\
Final-Recipient: rfc822; reader@example.invalid\r\nDisposition: manual-action/MDN-sent-manually; displayed\r\n\r\n--m--\r\n";
    let (message, _) = map(raw);
    assert_eq!(message["message_class"], "REPORT.IPM.Note.IPNRN");
    assert_eq!(message["to"][0]["email"], "sender@example.invalid");
}

#[test]
fn search_key_depends_on_container_and_ordinal() {
    let a = Origin {
        container: "Inbox.mbox".into(),
        ordinal: 3,
        ..Origin::default()
    };
    let b = Origin {
        container: "Inbox.mbox".into(),
        ordinal: 4,
        ..Origin::default()
    };
    let c = Origin {
        container: "Archive.mbox".into(),
        ordinal: 3,
        ..Origin::default()
    };
    let key = |origin: &Origin| {
        map_bytes(SIMPLE.as_bytes(), origin).0.message["search_key"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(key(&a).len(), 32);
    assert!(key(&a).bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(key(&a), key(&a.clone()));
    assert_ne!(key(&a), key(&b));
    assert_ne!(key(&a), key(&c));
}

#[test]
fn unreadable_message_is_kept_as_an_eml_attachment() {
    let (item, stage) = map_bytes(b"\x00\x01 not a message", &origin());
    assert_eq!(item.message["message_class"], "IPM.Note");
    assert_eq!(item.message["subject"], "");
    let attachments = item.message["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["name"], "message.eml");
    assert_eq!(staged(&stage, &attachments[0]), b"\x00\x01 not a message");
    assert_eq!(item.warnings.len(), 1);
}

#[test]
fn nul_bytes_and_huge_headers_do_not_panic() {
    let mut raw = b"From: a@example.invalid\r\nSubject: nul\x00byte\r\nX-Big: ".to_vec();
    raw.extend(std::iter::repeat_n(b'x', 10 * 1024 * 1024));
    raw.extend_from_slice(
        b"\r\nContent-Type: multipart/mixed; boundary=never\r\n\r\nno boundary ever\r\n",
    );
    let (item, _) = map_bytes(&raw, &origin());
    assert!(item.message["transport_headers"].as_str().unwrap().len() > 10 * 1024 * 1024);
}

#[test]
fn turkish_windows_1254_subject() {
    let raw = b"From: a@example.invalid\r\nSubject: =?windows-1254?Q?=DEubat_raporu_=FC=E7=FC=FE?=\r\nContent-Type: text/plain; charset=windows-1254\r\n\r\n\xc7al\xfd\xfemalar\r\n";
    let (item, _) = map_bytes(raw, &origin());
    assert_eq!(item.message["subject"], "Şubat raporu üçüş");
    assert_eq!(item.message["body"], "Çalışmalar\r\n");
}

#[test]
fn every_key_of_the_nested_form_is_present() {
    let (message, _) = map(SIMPLE);
    let keys: Vec<&str> = message
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for key in [
        "search_key",
        "message_id",
        "message_class",
        "subject",
        "sender",
        "to",
        "cc",
        "bcc",
        "body",
        "html",
        "sent_time",
        "received_time",
        "created_time",
        "last_modified_time",
        "is_draft",
        "is_read",
        "importance",
        "categories",
        "transport_headers",
        "properties",
        "attachments",
    ] {
        assert!(keys.contains(&key), "{key}");
    }
    assert_eq!(keys.len(), 21);
}

#[test]
fn deeply_nested_multiparts_do_not_overflow_the_stack() {
    let depth = 200_000;
    let mut raw = String::from("From: a@example.invalid\r\nSubject: deep\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b0\r\n\r\n");
    for level in 1..depth {
        raw.push_str(&format!(
            "--b{}\r\nContent-Type: multipart/mixed; boundary=b{level}\r\n\r\n",
            level - 1
        ));
    }
    raw.push_str(&format!(
        "--b{}\r\nContent-Type: text/plain\r\n\r\ninnermost\r\n",
        depth - 1
    ));
    // Run on a small stack, as a worker thread may have.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || map(&raw).0["body"].clone())
        .unwrap();
    let body = handle.join().expect("no stack overflow");
    assert!(body.as_str().unwrap().contains("innermost"));
}

fn with_part(part: &str) -> String {
    format!("From: a@example.invalid\r\nSubject: parts\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nBody\r\n--m\r\n{part}\r\n--m--\r\n")
}

#[test]
fn empty_attachment_name_gets_a_generated_name() {
    let raw = with_part("Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"=?utf-8?q??=\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0=");
    let (message, _) = map(&raw);
    assert_eq!(message["attachments"][0]["name"], "attachment-1.pdf");
}

#[test]
fn attached_message_named_dot_eml_without_subject_is_named_message() {
    let raw = with_part("Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\".eml\"\r\n\r\nFrom: b@example.invalid\r\n\r\nno subject");
    let (message, _) = map(&raw);
    assert_eq!(message["attachments"][0]["kind"], "message");
    assert_eq!(message["attachments"][0]["name"], "message");
}

#[test]
fn deep_unencoded_rfc822_nesting_does_not_overflow_the_stack() {
    let mut raw = String::from("From: a@example.invalid\r\nSubject: deep\r\n");
    for _ in 0..100_000 {
        raw.push_str("Content-Type: message/rfc822\r\n\r\nFrom: a@example.invalid\r\n");
    }
    raw.push_str("Subject: bottom\r\n\r\nbottom\r\n");
    // A 1 MiB stack, as the main thread has on Windows.
    let handle = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let (item, _) = map_bytes(raw.as_bytes(), &origin());
            depth_of(&item.message)
        })
        .unwrap();
    assert_eq!(handle.join().expect("no stack overflow"), MAX_NESTING);
}

#[test]
fn inline_text_part_with_a_filename_is_an_attachment() {
    let raw = with_part("Content-Type: text/plain; charset=utf-8\r\nContent-Disposition: inline; filename=notes.txt\r\n\r\nnotes");
    let (message, stage) = map(&raw);
    assert_eq!(message["body"], "Body");
    assert_eq!(message["attachments"][0]["name"], "notes.txt");
    assert_eq!(staged(&stage, &message["attachments"][0]), b"notes");
}

#[test]
fn unknown_charset_body_is_decoded_as_windows_1252() {
    let raw = b"From: a@example.invalid\r\nSubject: legacy\r\nContent-Type: text/plain; charset=unknown-8bit\r\n\r\ncaf\xe9 \x80\r\n";
    let (item, _) = map_bytes(raw, &origin());
    assert_eq!(item.message["body"], "caf\u{e9} \u{20ac}\r\n");
    let raw = b"From: a@example.invalid\r\nSubject: no charset\r\n\r\ncaf\xe9\r\n";
    assert_eq!(map_bytes(raw, &origin()).0.message["body"], "caf\u{e9}\r\n");
    let utf8 = "From: a@example.invalid\r\nSubject: utf8\r\n\r\ncafé\r\n";
    assert_eq!(map(utf8).0["body"], "café\r\n");
}

#[test]
fn header_block_decodes_each_line_on_its_own() {
    let raw = b"From: a@example.invalid\r\nX-A: caf\xc3\xa9\r\nX-B: caf\xe9\r\nSubject: s\r\n\r\nbody\r\n";
    let (item, _) = map_bytes(raw, &origin());
    let headers = item.message["transport_headers"].as_str().unwrap();
    assert!(headers.contains("X-A: caf\u{e9}\r\n"), "{headers}");
    assert!(headers.contains("X-B: caf\u{e9}\r\n"), "{headers}");
}

#[test]
fn cid_references_match_any_scheme_case_and_percent_encoding() {
    let raw = ALTERNATIVE.replace(
        "src=\"cid:logo@example.invalid\"",
        "src=\"CID:logo%40example.invalid\"",
    );
    let (message, _) = map(&raw);
    assert_eq!(message["attachments"][0]["inline"], true);
    assert_eq!(message["attachments"][1]["inline"], false);
}

#[test]
fn signed_message_with_an_8bit_content_type_keeps_its_boundary() {
    let raw = SIGNED
        .replace("micalg=sha-256;", "micalg=sha-256; x-note=caf\u{e9};")
        .into_bytes();
    let raw: Vec<u8> = String::from_utf8(raw)
        .unwrap()
        .replace('\u{e9}', "\u{0}")
        .bytes()
        .map(|b| if b == 0 { 0xe9 } else { b })
        .collect();
    let (item, stage) = map_bytes(&raw, &origin());
    let p7m = staged(&stage, &item.message["attachments"][0]);
    assert!(p7m.starts_with(b"Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=sha-256; x-note=caf\xe9;\r\n boundary=s\r\n\r\n--s\r\n"), "{}", String::from_utf8_lossy(&p7m[..80]));
}

#[test]
fn dsn_recipient_without_final_recipient_is_skipped() {
    let raw = DSN.replace("Final-Recipient: rfc822; a@example.invalid\r\n", "");
    let (message, _) = map(&raw);
    assert_eq!(message["message_class"], "REPORT.IPM.Note.NDR");
    assert_eq!(message["to"], json!([]));
}

#[test]
fn in_reply_to_and_references_are_written() {
    let raw = SIMPLE.replace("Subject:", "In-Reply-To: <parent@example.invalid>\r\nReferences: <root@example.invalid>\r\n <parent@example.invalid>\r\nSubject:");
    let (message, _) = map(&raw);
    let properties = message["properties"].as_array().unwrap();
    let value = |tag: &str| {
        properties
            .iter()
            .find(|p| p["tag"] == tag)
            .map(|p| p["value"].clone())
    };
    assert_eq!(value("0x1042"), Some(json!("<parent@example.invalid>")));
    assert_eq!(
        value("0x1039"),
        Some(json!("<root@example.invalid> <parent@example.invalid>"))
    );
}

#[test]
fn mdn_original_message_id_wins_over_references() {
    let raw = "From: reader@example.invalid\r\nTo: sender@example.invalid\r\nSubject: Read: hello\r\nReferences: <other@example.invalid>\r\n\
Date: Mon, 01 Jan 2024 00:00:00 +0000\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/report; report-type=disposition-notification; boundary=m\r\n\r\n\
--m\r\nContent-Type: text/plain\r\n\r\nRead\r\n--m\r\nContent-Type: message/disposition-notification\r\n\r\n\
Final-Recipient: rfc822; reader@example.invalid\r\nOriginal-Message-ID: <original@example.invalid>\r\nDisposition: manual-action/MDN-sent-manually; displayed\r\n\r\n--m--\r\n";
    let (message, _) = map(raw);
    let tags: Vec<&str> = message["properties"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["tag"].as_str().unwrap())
        .collect();
    assert_eq!(
        tags.iter().filter(|tag| **tag == "0x1039").count(),
        1,
        "{tags:?}"
    );
    assert_eq!(
        message["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["tag"] == "0x1039")
            .unwrap()["value"],
        "<original@example.invalid>"
    );
}

#[test]
fn huge_keywords_header_is_fast() {
    let keywords: Vec<String> = (0..200_000).map(|n| format!("k{n}")).collect();
    let raw = SIMPLE.replace(
        "Subject:",
        &format!("Keywords: {}\r\nSubject:", keywords.join(", ")),
    );
    let start = std::time::Instant::now();
    let (message, _) = map(&raw);
    assert_eq!(message["categories"].as_array().unwrap().len(), 200_000);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}
