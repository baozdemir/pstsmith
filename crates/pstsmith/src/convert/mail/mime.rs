//! One MIME message mapped to the manifest's nested-message form (`pst-export-manifest/2`, an item without its
//! `item_key` and `folder_path`), following MS-OXCMAIL.
//!
//! Nothing of the message is dropped: every leaf part becomes the plain text body, the HTML body or an attachment. The
//! plain text parts that are not attachments are joined into the body, the HTML parts into the HTML body; an attached
//! e-mail (message/rfc822) becomes an attached message, and every other part a file attachment with its bytes as they
//! were before the transfer encoding (a text attachment keeps its own charset). An attached e-mail deeper than
//! [`MAX_NESTING`] levels, and a message the parser cannot read at all, are kept as `.eml` file attachments.

use super::{labels, report, Origin, Stage};
use mail_parser::decoders::{
    base64::base64_decode, charsets::map::charset_decoder,
    quoted_printable::quoted_printable_decode,
};
use mail_parser::{
    Address, Encoding, HeaderValue, Message, MessageParser, MessagePart, MimeHeaders, PartType,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io;

/// Attached e-mails nest at most this many levels below the item, the writer's bound.
pub(crate) const MAX_NESTING: usize = 32;

/// Seconds since 1970 of 1601-01-01 and 9999-12-31 23:59:59 UTC, the range a PST time holds.
const MIN_UNIX_SECONDS: i64 = -11_644_473_600;
const MAX_UNIX_SECONDS: i64 = 253_402_300_799;

/// A mapped message.
pub(crate) struct MailItem {
    /// The message in the manifest's nested-message form.
    pub(crate) message: Value,
    /// The folder Gmail labels place the message in, if it has labels.
    pub(crate) folder: Option<String>,
    /// What was kept in another form than a mail client shows it (an unreadable message, a nesting too deep).
    pub(crate) warnings: Vec<String>,
}

fn parser() -> MessageParser {
    // Headers the parser does not know (X-Gmail-Labels and the like) are decoded as unstructured text.
    MessageParser::new().default_header_text()
}

/// Maps one message. `raw` is the message as stored (an `.eml` file, one MBOX entry without its "From " line).
pub(crate) fn message_from_mime(
    raw: &[u8],
    origin: &Origin,
    stage: &mut dyn Stage,
) -> io::Result<MailItem> {
    let mut hasher = Sha256::new();
    hasher.update(b"pstsmith search key\0");
    hasher.update(origin.container.as_bytes());
    hasher.update(b"\0");
    hasher.update(origin.ordinal.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(raw);
    let search_key = hex16(&hasher.finalize());
    let mut warnings = Vec::new();
    let mbox_date = origin
        .mbox_date
        .filter(|time| (MIN_UNIX_SECONDS..=MAX_UNIX_SECONDS).contains(time));
    let Some(message) = parser().parse(raw).filter(readable) else {
        warnings.push("unreadable message kept as message.eml".to_string());
        let eml = file_attachment(stage, raw, "message.eml", "message/rfc822", None, false)?;
        let mapped = json!({
            "search_key": search_key, "message_id": null, "message_class": "IPM.Note", "subject": "", "sender": null,
            "to": [], "cc": [], "bcc": [], "body": "", "html": null, "sent_time": null, "received_time": mbox_date,
            "created_time": mbox_date.unwrap_or(0), "last_modified_time": mbox_date.unwrap_or(0), "is_draft": false,
            "is_read": origin.seen.unwrap_or(true), "importance": 1, "categories": [], "transport_headers": null,
            "properties": [], "attachments": [eml],
        });
        return Ok(MailItem {
            message: mapped,
            folder: None,
            warnings,
        });
    };
    let labels = text_header(&message, "X-Gmail-Labels").map(labels::parse);
    let mut mapped = map(&message, &search_key, 0, stage, &mut warnings)?;
    // The source's own state: Maildir flags, then Gmail labels, then the headers mail clients write. Thunderbird keeps
    // X-Mozilla-Draft-Info on messages it sent, so that header says nothing about a draft.
    let draft = origin.draft || labels.as_ref().is_some_and(|labels| labels.draft);
    let read = origin.seen.unwrap_or_else(|| {
        if labels.as_ref().is_some_and(|labels| labels.unread) {
            return false;
        }
        if let Some(status) = text_header(&message, "X-Mozilla-Status")
            .and_then(|value| u32::from_str_radix(value.trim(), 16).ok())
        {
            return status & 0x0001 != 0;
        }
        // mbox Status R is read; X-Status A (answered) means it was read too.
        match (
            text_header(&message, "Status"),
            text_header(&message, "X-Status"),
        ) {
            (None, None) => true,
            (status, x_status) => {
                x_status.is_some_and(|value| value.contains('A'))
                    || status
                        .into_iter()
                        .chain(x_status)
                        .any(|value| value.contains('R'))
            }
        }
    });
    let sent = unix_time(message.date());
    let received = topmost_received(&message);
    let (sent_time, received_time) = if draft {
        (None, None)
    } else {
        (sent, received.or(mbox_date).or(sent))
    };
    let created = received_time.or(sent).or(mbox_date).unwrap_or(0);
    mapped["is_draft"] = json!(draft);
    mapped["is_read"] = json!(read);
    mapped["sent_time"] = json!(sent_time);
    mapped["received_time"] = json!(received_time);
    mapped["created_time"] = json!(created);
    mapped["last_modified_time"] = json!(created);
    if let Some(labels) = &labels {
        let categories = mapped["categories"]
            .as_array_mut()
            .expect("categories is a list");
        let mut seen: HashSet<String> = categories
            .iter()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        for category in &labels.categories {
            if seen.insert(category.clone()) {
                categories.push(json!(category));
            }
        }
    }
    drop_flat(message);
    Ok(MailItem {
        message: mapped,
        folder: labels.map(|labels| labels.folder),
        warnings,
    })
}

/// Drops a parsed message without recursion: the parser keeps an attached e-mail inside its parent, as deep as the
/// input nests them, and the derived recursive drop of a hostile message would overflow the stack.
fn drop_flat(message: Message<'_>) {
    let mut stack = vec![message.parts];
    while let Some(mut parts) = stack.pop() {
        for part in &mut parts {
            if let PartType::Message(nested) = std::mem::take(&mut part.body) {
                stack.push(nested.parts);
            }
        }
    }
}

/// A message has at least one header field with a well-formed name (RFC 5322 2.2: printable ASCII except the colon);
/// bytes without one are not a message the parser read.
fn readable(message: &Message<'_>) -> bool {
    message.headers().iter().any(|header| {
        !header.name().is_empty()
            && header
                .name()
                .bytes()
                .all(|byte| (33..=126).contains(&byte) && byte != b':')
    })
}

/// A header value without its folding (RFC 5322 2.2.3) and surrounding white space.
fn unfold(value: &str) -> String {
    value
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn hex16(digest: &[u8]) -> String {
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A header's decoded text; the first one when it is given several times.
fn text_header<'a>(message: &'a Message<'_>, name: &'static str) -> Option<&'a str> {
    match message.header_values(name).next()? {
        HeaderValue::Text(text) => Some(text.as_ref()),
        HeaderValue::TextList(list) => list.first().map(|text| text.as_ref()),
        _ => message.header_raw(name).map(str::trim),
    }
}

/// The date of the topmost Received header that has one: the last hop, the delivery.
fn topmost_received(message: &Message<'_>) -> Option<i64> {
    message
        .received_all()
        .find_map(|received| unix_time(received.date.as_ref()))
}

fn unix_time(date: Option<&mail_parser::DateTime>) -> Option<i64> {
    date.filter(|date| date.is_valid())
        .map(mail_parser::DateTime::to_timestamp)
        .filter(|time| (MIN_UNIX_SECONDS..=MAX_UNIX_SECONDS).contains(time))
}

fn addresses<'a>(lists: impl Iterator<Item = &'a Address<'a>>) -> Vec<Value> {
    let mut out = Vec::new();
    for list in lists {
        let addrs: Box<dyn Iterator<Item = &mail_parser::Addr<'_>>> = match list {
            Address::List(list) => Box::new(list.iter()),
            Address::Group(groups) => {
                Box::new(groups.iter().flat_map(|group| group.addresses.iter()))
            }
        };
        for addr in addrs {
            if let Some(email) = addr.address().filter(|email| !email.trim().is_empty()) {
                out.push(json!({ "name": addr.name().unwrap_or("").trim(), "email": email.trim(), "properties": [] }));
            }
        }
    }
    out
}

/// MS-OXCMAIL 2.2.3.2.5: the Importance header, else X-Priority.
fn importance(message: &Message<'_>) -> u8 {
    if let Some(value) = text_header(message, "Importance") {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => return 0,
            "normal" => return 1,
            "high" => return 2,
            _ => {}
        }
    }
    match text_header(message, "X-Priority").and_then(|value| value.trim().chars().next()) {
        Some('1' | '2') => 2,
        Some('4' | '5') => 0,
        _ => 1,
    }
}

/// The Keywords header's comma-separated values, each once.
fn keywords(message: &Message<'_>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    let values: Vec<String> = match message.keywords() {
        HeaderValue::TextList(list) => list.iter().map(|text| text.to_string()).collect(),
        HeaderValue::Text(text) => vec![text.to_string()],
        _ => Vec::new(),
    };
    for value in values {
        for keyword in value
            .split(',')
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty())
        {
            if seen.insert(keyword.to_string()) {
                out.push(json!(keyword));
            }
        }
    }
    out
}

/// The header block as stored: every header line, without the blank line that ends the block. Each line is read as
/// UTF-8, or as windows-1252 when it is not UTF-8.
fn header_block(message: &Message<'_>) -> Option<String> {
    let raw: &[u8] = message.raw_message.as_ref();
    let root = message.root_part();
    let mut block = raw.get(root.raw_header_offset() as usize..root.raw_body_offset() as usize)?;
    for ending in [&b"\r\n"[..], b"\n"] {
        if block.len() >= 2 * ending.len()
            && block.ends_with(ending)
            && block[..block.len() - ending.len()].ends_with(ending)
        {
            block = &block[..block.len() - ending.len()];
            break;
        }
    }
    if block.is_empty() {
        return None;
    }
    Some(
        block
            .split_inclusive(|byte| *byte == b'\n')
            .map(|line| decode_text(line).0)
            .collect(),
    )
}

/// Bytes as UTF-8, or as windows-1252 when they are not UTF-8 (the second value says which).
fn decode_text(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), false),
        Err(_) => (
            charset_decoder(b"windows-1252").map_or_else(
                || String::from_utf8_lossy(bytes).into_owned(),
                |decode| decode(bytes),
            ),
            true,
        ),
    }
}

fn content_type(part: &MessagePart<'_>) -> (String, String) {
    match part.content_type() {
        Some(ct) => (
            ct.ctype().to_ascii_lowercase(),
            ct.subtype().unwrap_or("").to_ascii_lowercase(),
        ),
        None => ("text".to_string(), "plain".to_string()),
    }
}

/// A part is an attachment when its disposition says so or when it has a file name, whatever its disposition: mail
/// clients send a named text file "inline" too.
fn is_attachment(part: &MessagePart<'_>) -> bool {
    part.content_disposition()
        .is_some_and(|disposition| disposition.ctype().eq_ignore_ascii_case("attachment"))
        || name_of(part).is_some()
}

/// A part's file name, if it has one that is not empty.
fn name_of<'a>(part: &'a MessagePart<'_>) -> Option<&'a str> {
    part.attachment_name()
        .filter(|name| !name.trim().is_empty())
}

/// The `cid:` references of an HTML body (RFC 2392: the scheme in any case, the Content-ID percent-encoded).
fn cid_references(html: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("cid:") {
        let start = from + at + 4;
        let end = html[start..]
            .find(|c: char| {
                c == '"' || c == '\'' || c == '<' || c == '>' || c == ')' || c.is_whitespace()
            })
            .map_or(html.len(), |n| start + n);
        out.insert(percent_decode(&html[start..end]));
        from = end.max(start);
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).and_then(|b| hex(*b)),
            bytes.get(i + 2).and_then(|b| hex(*b)),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A part's bytes before the transfer encoding: a text part keeps its own charset rather than the parser's Unicode.
fn part_bytes(raw: &[u8], part: &MessagePart<'_>) -> Vec<u8> {
    match &part.body {
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
        PartType::Message(message) => message.raw_message().to_vec(),
        PartType::Text(text) | PartType::Html(text) => {
            let body = raw
                .get(part.raw_body_offset() as usize..part.raw_end_offset() as usize)
                .unwrap_or(&[]);
            let decoded = match part.encoding {
                Encoding::Base64 => base64_decode(body),
                Encoding::QuotedPrintable => quoted_printable_decode(body),
                Encoding::None => Some(body.to_vec()),
            };
            decoded.unwrap_or_else(|| text.as_bytes().to_vec())
        }
        PartType::Multipart(_) => raw
            .get(part.raw_body_offset() as usize..part.raw_end_offset() as usize)
            .unwrap_or(&[])
            .to_vec(),
    }
}

/// The extension a nameless attachment of this type is given, so mail clients open it with the right program.
fn extension(mime: &str) -> &'static str {
    match mime {
        "text/plain"
        | "message/delivery-status"
        | "message/disposition-notification"
        | "text/rfc822-headers" => ".txt",
        "text/html" => ".html",
        "text/calendar" => ".ics",
        "text/vcard" | "text/x-vcard" => ".vcf",
        "application/pdf" => ".pdf",
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "message/rfc822" => ".eml",
        _ => "",
    }
}

fn file_attachment(
    stage: &mut dyn Stage,
    data: &[u8],
    name: &str,
    mime: &str,
    content_id: Option<&str>,
    inline: bool,
) -> io::Result<Value> {
    let staged = stage.stage(data)?;
    Ok(json!({
        "kind": "file", "path": staged.path, "name": name, "mime": mime, "content_id": content_id, "inline": inline,
        "size": staged.size, "sha256": staged.sha256,
    }))
}

/// A leaf of the MIME tree, in document order.
enum Leaf<'p> {
    Text(&'p MessagePart<'p>),
    Html(&'p MessagePart<'p>),
    File(&'p MessagePart<'p>),
    Message(&'p MessagePart<'p>),
}

fn leaves<'p>(message: &'p Message<'p>, root: usize, out: &mut Vec<Leaf<'p>>, seen: &mut [bool]) {
    // An explicit stack, not recursion: a hostile message may nest multiparts as deep as its size allows. Children are
    // pushed in reverse, so leaves come out in document order.
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some(part) = message.parts.get(id) else {
            continue;
        };
        // A part is visited once, whatever its parents say.
        if std::mem::replace(&mut seen[id], true) {
            continue;
        }
        match &part.body {
            PartType::Multipart(children) => {
                stack.extend(children.iter().rev().map(|child| *child as usize))
            }
            PartType::Message(_) => out.push(Leaf::Message(part)),
            _ => {
                let (kind, subtype) = content_type(part);
                if kind == "message" && subtype == "rfc822" {
                    out.push(Leaf::Message(part));
                } else if is_attachment(part) {
                    out.push(Leaf::File(part));
                } else if kind == "text"
                    && subtype == "plain"
                    && matches!(part.body, PartType::Text(_))
                {
                    out.push(Leaf::Text(part));
                } else if kind == "text"
                    && subtype == "html"
                    && matches!(part.body, PartType::Html(_) | PartType::Text(_))
                {
                    out.push(Leaf::Html(part));
                } else {
                    out.push(Leaf::File(part));
                }
            }
        }
    }
}

/// A body part's text. A charset the parser knows (other than UTF-8) is trusted; otherwise the bytes are read as
/// UTF-8, or as windows-1252 when they are not UTF-8, so a legacy Latin-1 or Turkish body keeps its letters.
fn text_of(raw: &[u8], part: &MessagePart<'_>, warnings: &mut Vec<String>) -> String {
    let charset = part.content_type().and_then(|ct| ct.attribute("charset"));
    let trusted = charset.is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        name != "utf-8" && name != "utf8" && charset_decoder(name.as_bytes()).is_some()
    });
    match &part.body {
        PartType::Text(text) | PartType::Html(text) if trusted => text.to_string(),
        _ => {
            let (text, legacy) = decode_text(&part_bytes(raw, part));
            if legacy {
                warnings
                    .push("a body without a usable charset was read as windows-1252".to_string());
            }
            text
        }
    }
}

/// The message in nested form. The top-level caller sets the origin's read state, draft state and times afterwards.
fn map(
    message: &Message<'_>,
    search_key: &str,
    depth: usize,
    stage: &mut dyn Stage,
    warnings: &mut Vec<String>,
) -> io::Result<Value> {
    // Part offsets index the bytes the parser was given: for an attached e-mail parsed inside its parent, the parent's.
    let raw: &[u8] = message.raw_message.as_ref();
    let root = message.root_part();
    let (kind, subtype) = content_type(root);
    let mut class = "IPM.Note".to_string();
    let mut properties = Vec::new();
    let mut to = addresses(
        message
            .header_values("To")
            .filter_map(HeaderValue::as_address),
    );
    let cc = addresses(
        message
            .header_values("Cc")
            .filter_map(HeaderValue::as_address),
    );
    let bcc = addresses(
        message
            .header_values("Bcc")
            .filter_map(HeaderValue::as_address),
    );
    let mut parts = Vec::new();
    let mut seen = vec![false; message.parts.len()];
    let mut attachments = Vec::new();
    let signed = kind == "multipart"
        && subtype == "signed"
        && root
            .content_type()
            .and_then(|ct| ct.attribute("protocol"))
            .is_some_and(|protocol| protocol.to_ascii_lowercase().contains("pkcs7-signature"));
    let opaque = kind == "application" && (subtype == "pkcs7-mime" || subtype == "x-pkcs7-mime");
    if signed {
        // MS-OXOSMIME 2.1.3: the protected content is the one smime.p7m attachment, the whole multipart/signed entity
        // with its Content-Type header field; the body is read from the signed content for display and search.
        class = "IPM.Note.SMIME.MultipartSigned".to_string();
        let field = root
            .headers
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case("Content-Type"))
            .and_then(|header| {
                raw.get(header.offset_field() as usize..header.offset_end() as usize)
            })
            .unwrap_or(b"Content-Type: multipart/signed\r\n");
        let body = raw
            .get(root.raw_body_offset() as usize..root.raw_end_offset() as usize)
            .unwrap_or(&[]);
        let mut entity = field.to_vec();
        if !entity.ends_with(b"\n") {
            entity.extend_from_slice(b"\r\n");
        }
        entity.extend_from_slice(b"\r\n");
        entity.extend_from_slice(body);
        attachments.push(file_attachment(
            stage,
            &entity,
            "smime.p7m",
            "multipart/signed",
            None,
            false,
        )?);
        if let PartType::Multipart(children) = &root.body {
            if let Some(first) = children.first() {
                leaves(message, *first as usize, &mut parts, &mut seen);
            }
        }
        parts.retain(|leaf| matches!(leaf, Leaf::Text(_) | Leaf::Html(_)));
    } else if opaque {
        // MS-OXOSMIME 2.1.3: an opaque-signed or encrypted message stores the CMS object as its one attachment.
        class = "IPM.Note.SMIME".to_string();
        attachments.push(file_attachment(
            stage,
            root.contents(),
            "smime.p7m",
            "application/pkcs7-mime",
            None,
            false,
        )?);
    } else {
        leaves(message, 0, &mut parts, &mut seen);
        if kind == "multipart" && subtype == "report" {
            let sent = unix_time(message.date()).map(|time| (time + 11_644_473_600) * 10_000_000);
            let status = parts.iter().find_map(|leaf| match leaf {
                Leaf::File(part) | Leaf::Text(part) => match content_type(part) {
                    (k, s)
                        if k == "message"
                            && (s == "delivery-status" || s == "disposition-notification") =>
                    {
                        Some((s, *part))
                    }
                    _ => None,
                },
                _ => None,
            });
            if let (Some(sent), Some((report_kind, part))) = (sent, status) {
                let text = String::from_utf8_lossy(&part_bytes(raw, part)).into_owned();
                let derived = if report_kind == "delivery-status" {
                    report::delivery_status(&text, sent)
                } else {
                    report::disposition(&text, sent)
                };
                if let Some(suffix) = derived.suffix {
                    class = format!("REPORT.IPM.Note{suffix}");
                    properties = derived.properties;
                    if report_kind == "delivery-status" {
                        // A delivery status notification names its recipients by address only (MS-OXCMAIL 2.2.3.7.1).
                        to = derived
                            .recipients
                            .into_iter()
                            .filter(|(email, _)| !email.is_empty())
                            .map(|(email, properties)| json!({ "name": email, "email": email, "properties": properties }))
                            .collect();
                    }
                }
            }
        }
    }
    let mut body = Vec::new();
    let mut html = Vec::new();
    for leaf in &parts {
        match leaf {
            Leaf::Text(part) => body.push(text_of(raw, part, warnings)),
            Leaf::Html(part) => html.push(text_of(raw, part, warnings)),
            _ => {}
        }
    }
    let html = if html.is_empty() {
        None
    } else {
        Some(html.concat())
    };
    let references = html.as_deref().map(cid_references).unwrap_or_default();
    let mut index = 0usize;
    for leaf in &parts {
        match leaf {
            Leaf::File(part) => {
                index += 1;
                let (kind, subtype) = content_type(part);
                let mime = format!("{kind}/{subtype}");
                let name = name_of(part)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("attachment-{index}{}", extension(&mime)));
                let content_id = part
                    .content_id()
                    .map(|id| id.trim_start_matches('<').trim_end_matches('>'))
                    .filter(|id| !id.is_empty());
                let inline = content_id.is_some_and(|id| references.contains(id));
                attachments.push(file_attachment(
                    stage,
                    &part_bytes(raw, part),
                    &name,
                    &mime,
                    content_id,
                    inline,
                )?);
            }
            Leaf::Message(part) => {
                index += 1;
                // An unencoded attached e-mail the parser parsed in place; an encoded one beyond its own depth, as bytes.
                let owned;
                let bytes: &[u8] = match &part.body {
                    PartType::Message(nested) => nested.raw_message(),
                    PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes,
                    _ => {
                        owned = part_bytes(raw, part);
                        &owned
                    }
                };
                let nested_key = {
                    let mut hasher = Sha256::new();
                    hasher.update(search_key.as_bytes());
                    hasher.update(format!("/{index}\0").as_bytes());
                    hasher.update(bytes);
                    hex16(&hasher.finalize())
                };
                let given_name = name_of(part)
                    .map(|name| name.strip_suffix(".eml").unwrap_or(name).to_string())
                    .filter(|name| !name.trim().is_empty());
                let deep = depth + 1 > MAX_NESTING;
                let reparsed = if deep || matches!(part.body, PartType::Message(_)) {
                    None
                } else {
                    parser().parse(bytes).filter(readable)
                };
                let nested = match &part.body {
                    _ if deep => None,
                    PartType::Message(nested) => Some(nested).filter(|nested| readable(nested)),
                    _ => reparsed.as_ref(),
                };
                match nested {
                    Some(nested) => {
                        let mut inner = map(nested, &nested_key, depth + 1, stage, warnings)?;
                        let subject = inner["subject"].as_str().unwrap_or("").to_string();
                        let name = if subject.trim().is_empty() {
                            given_name.unwrap_or_else(|| "message".to_string())
                        } else {
                            subject
                        };
                        let sent = unix_time(nested.date());
                        let received = topmost_received(nested).or(sent);
                        inner["sent_time"] = json!(sent);
                        inner["received_time"] = json!(received);
                        inner["created_time"] = json!(received.unwrap_or(0));
                        inner["last_modified_time"] = json!(received.unwrap_or(0));
                        attachments.push(json!({ "kind": "message", "name": name, "inline": false, "message": inner }));
                    }
                    None => {
                        if deep {
                            warnings.push(format!("attached e-mail nested deeper than {MAX_NESTING} levels kept as an .eml file"));
                        } else {
                            warnings.push(
                                "unreadable attached e-mail kept as an .eml file".to_string(),
                            );
                        }
                        let name = format!(
                            "{}.eml",
                            given_name.unwrap_or_else(|| "message".to_string())
                        );
                        attachments.push(file_attachment(
                            stage,
                            bytes,
                            &name,
                            "message/rfc822",
                            None,
                            false,
                        )?);
                    }
                }
                if let Some(reparsed) = reparsed {
                    drop_flat(reparsed);
                }
            }
            _ => {}
        }
    }
    for (tag, name) in [(0x1042u16, "In-Reply-To"), (0x1039u16, "References")] {
        let tag_text = format!("0x{tag:04X}");
        if properties
            .iter()
            .any(|property: &Value| property["tag"] == tag_text.as_str())
        {
            continue; // a disposition report gives its own (MS-OXCMAIL 2.2.3.7.2)
        }
        if let Some(value) = message
            .header_raw(name)
            .map(unfold)
            .filter(|value| !value.is_empty())
        {
            properties.push(report::prop(tag, "string", json!(value)));
        }
    }
    let sender = message
        .from()
        .and_then(|from| addresses(std::iter::once(from)).into_iter().next())
        .map(|mut address| {
            address
                .as_object_mut()
                .expect("an address is an object")
                .remove("properties");
            address
        });
    Ok(json!({
        "search_key": search_key,
        "message_id": message.message_id().filter(|id| !id.trim().is_empty()).map(|id| format!("<{}>", id.trim())),
        "message_class": class,
        "subject": message.subject().unwrap_or(""),
        "sender": sender,
        "to": to,
        "cc": cc,
        "bcc": bcc,
        "body": body.join("\n"),
        "html": html,
        "sent_time": null,
        "received_time": null,
        "created_time": 0,
        "last_modified_time": 0,
        "is_draft": false,
        "is_read": true,
        "importance": importance(message),
        "categories": keywords(message),
        "transport_headers": header_block(message),
        "properties": properties,
        "attachments": attachments,
    }))
}
