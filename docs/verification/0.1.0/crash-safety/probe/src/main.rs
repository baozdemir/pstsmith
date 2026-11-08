//! Appends batches of messages to a PST with pstsmith's writer, one commit per batch, and prints a line after each
//! commit returns. The crash and out-of-space checks run it (see ../README.md).
//!
//!     append-probe create FILE.pst
//!     append-probe append FILE.pst BATCHES
//!
//! `create` writes a new file with pstsmith's template. `append` opens FILE.pst, appends BATCHES batches of 10
//! messages and prints `ok N` once the N-th batch is committed. The batches go to Inbox, Probe/A and Probe/B in turn (a
//! missing folder is created by the first batch that needs it, in its own commit); every third message has an
//! attachment of up to 160 KB. Any error prints `error KIND: MESSAGE` and exits with status 1.
use pstsmith::{
    UnicodePstAttachment, UnicodePstBatchMessage, UnicodePstImportance, UnicodePstMessage,
    UnicodePstRecipient, UnicodePstRecipientType, UnicodePstSender, UnicodePstWriter,
};
use std::io;

const MESSAGES_PER_BATCH: usize = 10;
const TIME: i64 = 133_750_080_000_000_000;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("create"), Some(path)) => {
            UnicodePstWriter::create(path).and_then(UnicodePstWriter::close)
        }
        (Some("append"), Some(path)) => {
            append(path, args.get(3).and_then(|n| n.parse().ok()).unwrap_or(1))
        }
        _ => {
            eprintln!("usage: append-probe create FILE.pst | append-probe append FILE.pst BATCHES");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        println!("error {:?}: {error}", error.kind());
        std::process::exit(1);
    }
}

fn append(path: &str, batches: usize) -> io::Result<()> {
    let mut writer = UnicodePstWriter::open(path)?;
    let run = std::process::id();
    let recipients = [UnicodePstRecipient {
        name: "to",
        email: "to@example.com",
        recipient_type: UnicodePstRecipientType::To,
        properties: &[],
    }];
    for batch in 1..=batches {
        let folder: &[&str] = match batch % 3 {
            0 => &["Inbox"],
            1 => &["Probe", "A"],
            _ => &["Probe", "B"],
        };
        let subjects: Vec<String> = (0..MESSAGES_PER_BATCH)
            .map(|i| format!("run {run} batch {batch} message {i}"))
            .collect();
        let ids: Vec<String> = (0..MESSAGES_PER_BATCH)
            .map(|i| format!("<{run}.{batch}.{i}@probe.example>"))
            .collect();
        let bodies: Vec<String> = (0..MESSAGES_PER_BATCH)
            .map(|i| "a line of body text\r\n".repeat(1 + (batch * 7 + i * 13) % 600))
            .collect();
        let data: Vec<Vec<u8>> = (0..MESSAGES_PER_BATCH)
            .map(|i| {
                let len = 1 + (batch + i) % 5 * 40_000;
                (0..len).map(|k| (k * 31 + batch) as u8).collect()
            })
            .collect();
        let attachments: Vec<Vec<UnicodePstAttachment>> = (0..MESSAGES_PER_BATCH)
            .map(|i| {
                if i % 3 != 0 {
                    return Vec::new();
                }
                vec![UnicodePstAttachment {
                    filename: "data.bin",
                    mime_type: "application/octet-stream",
                    content_id: None,
                    data: &data[i],
                    stream: None,
                    hidden: false,
                    attached_message: None,
                }]
            })
            .collect();
        let messages: Vec<UnicodePstBatchMessage> = (0..MESSAGES_PER_BATCH)
            .map(|i| UnicodePstBatchMessage {
                message: UnicodePstMessage {
                    message_class: "IPM.Note",
                    subject: &subjects[i],
                    sender: Some(UnicodePstSender {
                        name: "sender",
                        email: "sender@example.com",
                        address_type: "SMTP",
                    }),
                    recipients: &recipients,
                    body: &bodies[i],
                    html_body: None,
                    message_id: Some(&ids[i]),
                    delivery_time: Some(TIME),
                    creation_time: TIME,
                    last_modification_time: TIME,
                    unsent: false,
                    properties: &[],
                    named_properties: &[],
                    search_key: None,
                    is_read: i % 2 == 0,
                    importance: UnicodePstImportance::Normal,
                    client_submit_time: Some(TIME),
                    categories: &[],
                    transport_headers: None,
                },
                attachments: &attachments[i],
            })
            .collect();
        writer.append_many_in_folder_with_attachments(folder, &messages)?;
        println!("ok {batch}");
    }
    writer.close()
}
