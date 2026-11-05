//! Report message properties from a report's MIME parts (MS-OXCMAIL 2.2.3.7).
//!
//! A delivery status notification (message/delivery-status, RFC 3464) and a message disposition notification
//! (message/disposition-notification, RFC 3798) become a report Message object: the class suffix, the report's own
//! properties and, for a delivery status notification, the recipients with their delivery status. The values are those
//! MS-OXCMAIL's MIME reader stores, in the manifest's property form (`{"tag", "type", "value"}`).

use serde_json::{json, Value};

/// 2.2.3.7.1.1: the Action field's severity, and the class suffix of the highest.
const SEVERITY: &[(&str, i32, &str)] = &[
    ("delivered", 0, ".DR"),
    ("expanded", 1, ".Expanded.DR"),
    ("relayed", 2, ".Relayed.DR"),
    ("delayed", 3, ".Delayed.DR"),
    ("failed", 4, ".NDR"),
];

/// 2.2.3.7.2: the disposition-type and the class suffix.
const MDN_SUFFIX: &[(&str, &str)] = &[
    ("displayed", ".IPNRN"),
    ("dispatched", ".IPNRN"),
    ("processed", ".IPNRN"),
    ("deleted", ".IPNNRN"),
    ("denied", ".IPNNRN"),
    ("failed", ".IPNNRN"),
];

/// 2.2.3.7.1.3: (Subject, Detail) -> (DiagnosticCode, ReasonCode); `None` keeps the value as it is. Detail `None` is
/// "any other value" of Detail. A Subject not in the table changes nothing.
#[allow(clippy::type_complexity)]
const CODES: &[((u8, Option<u8>), (Option<i64>, Option<i64>))] = &[
    ((1, Some(1)), (Some(35), Some(1))),
    ((1, Some(2)), (Some(48), None)),
    ((1, Some(3)), (Some(32), None)),
    ((1, Some(4)), (Some(1), None)),
    ((1, Some(6)), (Some(40), None)),
    ((1, None), (Some(0), None)),
    ((2, Some(1)), (Some(38), None)),
    ((2, Some(2)), (Some(13), None)),
    ((2, Some(3)), (Some(13), None)),
    ((2, Some(4)), (Some(30), None)),
    ((2, None), (Some(38), None)),
    ((3, Some(1)), (Some(38), None)),
    ((3, Some(2)), (None, None)),
    ((3, Some(3)), (Some(18), None)),
    ((3, Some(4)), (Some(13), None)),
    ((3, Some(5)), (Some(18), None)),
    ((3, None), (Some(38), None)),
    ((4, Some(0)), (None, None)),
    ((4, Some(3)), (None, Some(6))),
    ((4, Some(4)), (None, None)),
    ((4, Some(6)), (Some(3), None)),
    ((4, Some(7)), (Some(5), None)),
    ((4, Some(8)), (Some(3), None)),
    ((4, None), (Some(2), None)),
    ((5, Some(3)), (Some(16), None)),
    ((5, Some(4)), (Some(11), None)),
    ((5, None), (Some(17), None)),
    ((6, Some(2)), (Some(9), None)),
    ((6, Some(3)), (Some(8), None)),
    ((6, Some(4)), (Some(25), None)),
    ((6, Some(5)), (None, Some(2))),
    ((6, None), (Some(15), None)),
    ((7, Some(1)), (Some(29), None)),
    ((7, Some(2)), (Some(28), None)),
    ((7, Some(3)), (Some(26), None)),
    ((7, None), (Some(46), None)),
];

/// The report a MIME report part describes.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Derived {
    /// The class suffix (`.NDR`, `.DR`, `.IPNRN`, ...), `None` when the part names no known action or disposition.
    pub(crate) suffix: Option<&'static str>,
    /// The report message's own properties, by tag.
    pub(crate) properties: Vec<Value>,
    /// Delivery status notifications: each reported recipient's address and properties, by tag.
    pub(crate) recipients: Vec<(String, Vec<Value>)>,
}

pub(crate) fn prop(tag: u16, kind: &str, value: Value) -> Value {
    json!({ "tag": format!("0x{tag:04X}"), "type": kind, "value": value })
}

fn by_tag(properties: &mut [Value]) {
    properties.sort_by(|a, b| a["tag"].as_str().cmp(&b["tag"].as_str()));
}

/// The part of an RFC 3464 typed field after its type and ';' ("rfc822; a@b" gives "a@b", "dns; mx" gives "mx").
fn after_type(value: &str) -> &str {
    value.split_once(';').map_or(value, |(_, rest)| rest).trim()
}

/// 2.2.3.7.1.3: (PidTagNonDeliveryReportStatusCode, DiagnosticCode, ReasonCode) from a Status field such as "5.1.1";
/// `None` when the field is not three dot-separated numbers.
pub(crate) fn status_codes(status: &str) -> Option<(i64, i64, i64)> {
    let code = status.trim().split(' ').next().unwrap_or("");
    let mut numbers = code.split('.').take(3).map(|part| part.parse::<i64>().ok());
    let (kind, subject, detail) = (numbers.next()??, numbers.next()??, numbers.next()??);
    // RFC 3464 2.3.4: the class is one digit. A longer one is no status, so the code always fits PtypInteger32.
    if !(0..=9).contains(&kind) {
        return None;
    }
    let subject = if (0..=9).contains(&subject) {
        subject as u8
    } else {
        0
    };
    let detail = if (0..=9).contains(&detail) {
        detail as u8
    } else {
        0
    };
    let (mut diagnostic, mut reason) = (-1, 0);
    if CODES.iter().any(|((s, _), _)| *s == subject) {
        let found = CODES
            .iter()
            .find(|((s, d), _)| *s == subject && *d == Some(detail))
            .or_else(|| {
                CODES
                    .iter()
                    .find(|((s, d), _)| *s == subject && d.is_none())
            })
            .map(|(_, codes)| *codes);
        if let Some((new_diagnostic, new_reason)) = found {
            diagnostic = new_diagnostic.unwrap_or(diagnostic);
            reason = new_reason.unwrap_or(reason);
        }
    }
    Some((
        kind * 100 + i64::from(subject) * 10 + i64::from(detail),
        diagnostic,
        reason,
    ))
}

/// The field blocks of a message/delivery-status or message/disposition-notification body, blank-line separated;
/// a folded line continues the field before it. A field given twice keeps its last value in its first place.
fn blocks(text: &str) -> Vec<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut current: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                index.clear();
            }
        } else if line.starts_with([' ', '\t']) && !current.is_empty() {
            let last = current.last_mut().expect("not empty");
            last.1.push(' ');
            last.1.push_str(line.trim());
        } else if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_string(), value.trim().to_string());
            match index.get(&name) {
                Some(&at) => current[at].1 = value,
                None => {
                    index.insert(name.clone(), current.len());
                    current.push((name, value));
                }
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn field<'a>(block: &'a [(String, String)], name: &str) -> Option<&'a str> {
    block
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// A message/delivery-status body; `sent` is the report's sent time (its Date header) in FILETIME units.
pub(crate) fn delivery_status(text: &str, sent: i64) -> Derived {
    let fields = blocks(text);
    let Some((per_message, per_recipient)) = fields.split_first() else {
        return Derived::default();
    };
    let severity = |action: &str| {
        SEVERITY
            .iter()
            .find(|(name, _, _)| *name == action)
            .map_or(-1, |(_, level, _)| *level)
    };
    let actions: Vec<String> = per_recipient
        .iter()
        .map(|block| field(block, "Action").unwrap_or("").to_lowercase())
        .collect();
    // Python's max keeps the first of equal maxima.
    let highest = actions
        .iter()
        .fold(None::<&String>, |best, action| match best {
            Some(best) if severity(best) >= severity(action) => Some(best),
            _ => Some(action),
        });
    let mut properties = Vec::new();
    if let Some(reporting) = field(per_message, "Reporting-MTA") {
        properties.push(prop(0x6820, "string", json!(reporting)));
    }
    let mut recipients = Vec::new();
    for (block, action) in per_recipient.iter().zip(&actions) {
        // 2.2.3.7.1 step 1: only the recipients of the highest severity.
        if Some(action) != highest {
            continue;
        }
        let mut recipient = Vec::new();
        let remote_name = field(block, "Remote-MTA").map(after_type);
        if let Some(remote_name) = remote_name {
            recipient.push(prop(0x0C21, "string", json!(remote_name)));
        }
        let status = field(block, "Status").unwrap_or("");
        let supplementary = match field(block, "X-Supplementary-Info") {
            Some(info) => info.to_string(),
            None => {
                let diagnostic = field(block, "Diagnostic-Code")
                    .map(|code| format!(" {code}"))
                    .unwrap_or_default();
                format!(
                    "<{} #{}{}>",
                    remote_name.unwrap_or(""),
                    status.split(' ').next().unwrap_or(""),
                    diagnostic
                )
            }
        };
        recipient.push(prop(0x0C1B, "string", json!(supplementary)));
        recipient.push(prop(0x0032, "time", json!(sent)));
        if action != "failed" {
            recipient.push(prop(0x0010, "time", json!(sent)));
        } else if let Some((code, diagnostic, reason)) = status_codes(status) {
            recipient.push(prop(0x0C20, "int32", json!(code)));
            recipient.push(prop(0x0C05, "int32", json!(diagnostic)));
            recipient.push(prop(0x0C04, "int32", json!(reason)));
        }
        by_tag(&mut recipient);
        recipients.push((
            after_type(field(block, "Final-Recipient").unwrap_or("")).to_string(),
            recipient,
        ));
    }
    let suffix = highest
        .and_then(|action| SEVERITY.iter().find(|(name, _, _)| name == action))
        .map(|(_, _, suffix)| *suffix);
    Derived {
        suffix,
        properties,
        recipients,
    }
}

/// `email.utils.parseaddr` for one address: ("Name", "address") from `Name <address>` or a bare address.
fn parse_address(value: &str) -> (String, String) {
    match (value.find('<'), value.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = value[..open].trim().trim_matches('"').trim();
            (name.to_string(), value[open + 1..close].trim().to_string())
        }
        _ => (String::new(), value.trim().to_string()),
    }
}

/// A message/disposition-notification body; `sent` is the report's sent time in FILETIME units.
pub(crate) fn disposition(text: &str, sent: i64) -> Derived {
    let Some(notification) = blocks(text).into_iter().next() else {
        return Derived::default();
    };
    let recipient = field(&notification, "Original-Recipient")
        .or_else(|| field(&notification, "Final-Recipient"))
        .unwrap_or("");
    let (name, address) = parse_address(after_type(recipient));
    let disposition = field(&notification, "Disposition").unwrap_or("");
    let mut properties = vec![
        prop(
            0x0074,
            "string",
            json!(if name.is_empty() { address } else { name }),
        ),
        prop(0x0055, "time", json!(sent)),
        prop(0x002A, "time", json!(sent)),
        prop(0x0032, "time", json!(sent)),
        prop(0x1001, "string", json!(disposition)),
    ];
    if let Some(key) =
        field(&notification, "X-MSExch-Correlation-Key").filter(|key| !key.is_empty())
    {
        if let Some(bytes) = mail_parser::decoders::base64::base64_decode(key.as_bytes()) {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            properties.push(prop(0x0025, "binary", json!(hex)));
        }
    }
    if let Some(original) = field(&notification, "Original-Message-ID").filter(|id| !id.is_empty())
    {
        properties.push(prop(0x1046, "string", json!(original)));
        properties.push(prop(0x1039, "string", json!(original)));
    }
    let kind = match disposition.split_once(';') {
        Some((_, rest)) => rest
            .trim()
            .split('/')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase(),
        None => disposition
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase(),
    };
    by_tag(&mut properties);
    let suffix = MDN_SUFFIX
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, suffix)| *suffix);
    Derived {
        suffix,
        properties,
        recipients: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENT: i64 = (1_704_067_200 + 11_644_473_600) * 10_000_000;

    fn values(properties: &[Value]) -> Vec<(String, Value)> {
        properties
            .iter()
            .map(|p| (p["tag"].as_str().unwrap().to_string(), p["value"].clone()))
            .collect()
    }

    fn value(properties: &[Value], tag: &str) -> Option<Value> {
        properties
            .iter()
            .find(|p| p["tag"] == tag)
            .map(|p| p["value"].clone())
    }

    #[test]
    fn status_codes_follow_the_section_table() {
        assert_eq!(status_codes("5.7.0"), Some((570, 46, 0)));
        assert_eq!(status_codes("5.1.1"), Some((511, 35, 1)));
        assert_eq!(status_codes("4.4.3"), Some((443, -1, 6)));
        assert_eq!(status_codes("5.6.5"), Some((565, -1, 2)));
        assert_eq!(status_codes("5.0.0"), Some((500, -1, 0)));
        assert_eq!(status_codes("5.1.12"), Some((510, 0, 0)));
        assert_eq!(status_codes("x.1.1"), None);
        // RFC 3464 2.3.4: the class is one digit; anything else is no status (the value is an int32).
        assert_eq!(status_codes("99999999999999999999.1.1"), None);
        assert_eq!(status_codes("9223372036854775807.1.1"), None);
        assert_eq!(status_codes("-5.1.1"), None);
        assert_eq!(status_codes("12.1.1"), None);
    }

    const DSN: &str = "Reporting-MTA: dns; mx.example.invalid\r\n\r\nFinal-Recipient: rfc822; a@example.invalid\r\n\
        Action: failed\r\nStatus: 5.1.1\r\nRemote-MTA: dns; remote.example.invalid\r\nDiagnostic-Code: smtp; 550 5.1.1 unknown\r\n\r\n\
        Final-Recipient: rfc822; b@example.invalid\r\nAction: delivered\r\nStatus: 2.0.0\r\n";

    #[test]
    fn dsn_report_class_and_recipient_properties() {
        let report = delivery_status(DSN, SENT);
        assert_eq!(report.suffix, Some(".NDR"));
        assert_eq!(report.recipients.len(), 1, "the delivered one is skipped");
        let (address, properties) = &report.recipients[0];
        assert_eq!(address, "a@example.invalid");
        assert_eq!(value(properties, "0x0C20"), Some(json!(511)));
        assert_eq!(value(properties, "0x0C05"), Some(json!(35)));
        assert_eq!(value(properties, "0x0C04"), Some(json!(1)));
        assert_eq!(
            value(properties, "0x0C21"),
            Some(json!("remote.example.invalid"))
        );
        assert_eq!(
            value(properties, "0x0C1B"),
            Some(json!(
                "<remote.example.invalid #5.1.1 smtp; 550 5.1.1 unknown>"
            ))
        );
        assert_eq!(value(properties, "0x0032"), Some(json!(SENT)));
        assert_eq!(value(properties, "0x0010"), None);
        assert_eq!(
            values(&report.properties),
            vec![("0x6820".to_string(), json!("dns; mx.example.invalid"))]
        );
        let tags: Vec<String> = values(properties).into_iter().map(|(tag, _)| tag).collect();
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(tags, sorted);
    }

    #[test]
    fn many_fields_in_one_block_are_fast() {
        let fields: String = (0..200_000)
            .map(|n| format!("X-Field-{n}: value\r\n"))
            .collect();
        let start = std::time::Instant::now();
        let report = delivery_status(
            &format!("{fields}\r\n{}", DSN.split_once("\r\n\r\n").unwrap().1),
            SENT,
        );
        assert_eq!(report.suffix, Some(".NDR"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn dsn_suffix_follows_the_highest_action() {
        for (action, suffix) in [
            ("delivered", ".DR"),
            ("relayed", ".Relayed.DR"),
            ("expanded", ".Expanded.DR"),
            ("delayed", ".Delayed.DR"),
        ] {
            let text = DSN.replace(
                "Action: failed\r\nStatus: 5.1.1",
                &format!("Action: {action}\r\nStatus: 2.0.0"),
            );
            let report = delivery_status(&text, SENT);
            assert_eq!(report.suffix, Some(suffix), "{action}");
            let properties = &report.recipients[0].1;
            assert_eq!(value(properties, "0x0010"), Some(json!(SENT)), "{action}");
            assert_eq!(value(properties, "0x0032"), Some(json!(SENT)), "{action}");
            assert_eq!(value(properties, "0x0C20"), None, "{action}");
        }
    }

    const MDN: &str = "Final-Recipient: rfc822; reader@example.invalid\r\nOriginal-Recipient: rfc822; Reader Name <reader@example.invalid>\r\n\
        Original-Message-ID: <original@example.invalid>\r\nX-MSExch-Correlation-Key: AQID\r\n\
        Disposition: manual-action/MDN-sent-manually; displayed\r\n";

    #[test]
    fn mdn_report_class() {
        let report = disposition(MDN, SENT);
        assert_eq!(report.suffix, Some(".IPNRN"));
        assert!(report.recipients.is_empty());
        assert_eq!(
            values(&report.properties),
            vec![
                ("0x0025".to_string(), json!("010203")),
                ("0x002A".to_string(), json!(SENT)),
                ("0x0032".to_string(), json!(SENT)),
                ("0x0055".to_string(), json!(SENT)),
                ("0x0074".to_string(), json!("Reader Name")),
                (
                    "0x1001".to_string(),
                    json!("manual-action/MDN-sent-manually; displayed")
                ),
                ("0x1039".to_string(), json!("<original@example.invalid>")),
                ("0x1046".to_string(), json!("<original@example.invalid>")),
            ]
        );
        assert_eq!(
            disposition(&MDN.replace("; displayed", "; deleted"), SENT).suffix,
            Some(".IPNNRN")
        );
        let plain = disposition(
            &MDN.replace(
                "Original-Recipient: rfc822; Reader Name <reader@example.invalid>\r\n",
                "",
            ),
            SENT,
        );
        assert_eq!(
            value(&plain.properties, "0x0074"),
            Some(json!("reader@example.invalid"))
        );
    }
}
