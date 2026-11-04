use pstsmith::{
    UnicodePstAttachment, UnicodePstFile, UnicodePstImportance, UnicodePstMessage,
    UnicodePstRecipient, UnicodePstRecipientType, UnicodePstSender,
};
use std::{env, io, path::PathBuf};

const INLINE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

fn main() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixture.pst"));
    let stress = args.any(|arg| arg == "--stress");
    let body = if stress {
        "Large Unicode body 본문. ".repeat(1_000)
    } else {
        "Hello from Memex PST export.".to_string()
    };
    let html = if stress {
        format!(
            "{}<img src=\"cid:memex-inline@example.com\">",
            "<p>Large HTML body 본문.</p>".repeat(1_000)
        )
    } else {
        "<p>Hello from <strong>Memex PST export</strong>.</p><img src=\"cid:memex-inline@example.com\">"
            .to_string()
    };
    let recipients = [
        UnicodePstRecipient {
            name: "Memex Recipient",
            email: "recipient@example.com",
            recipient_type: UnicodePstRecipientType::To,
            properties: &[],
        },
        UnicodePstRecipient {
            name: "Memex CC",
            email: "cc@example.com",
            recipient_type: UnicodePstRecipientType::Cc,
            properties: &[],
        },
        UnicodePstRecipient {
            name: "Memex BCC",
            email: "bcc@example.com",
            recipient_type: UnicodePstRecipientType::Bcc,
            properties: &[],
        },
    ];
    let message = UnicodePstMessage {
        subject: "Memex PST export",
        recipients: &recipients,
        body: &body,
        html_body: Some(&html),
        message_id: Some("<memex-pst-export@example.com>"),
        delivery_time: Some(133_750_080_000_000_000),
        creation_time: 133_750_080_000_000_000,
        last_modification_time: 133_750_080_000_000_000,
        search_key: None,
        is_read: true,
        importance: UnicodePstImportance::Normal,
        sender: Some(UnicodePstSender { name: "Memex Sender", email: "sender@example.com", address_type: "SMTP" }),
        client_submit_time: None,
        categories: &[],
        transport_headers: None,
        message_class: "IPM.Note",
        unsent: false,
        named_properties: &[],
        properties: &[],
    };
    let inline_attachment = UnicodePstAttachment {
        filename: "inline.png",
        mime_type: "image/png",
        content_id: Some("<memex-inline@example.com>"),
        data: INLINE_PNG,
        stream: None,
        hidden: false,
        attached_message: None,
    };
    if stress {
        let data = vec![0xA5; 320 * 1_024];
        let attachment = UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &data,
            stream: None,
            hidden: false,
            attached_message: None,
        };
        let empty_attachment = UnicodePstAttachment {
            filename: "empty.txt",
            mime_type: "text/plain",
            content_id: None,
            data: &[],
            stream: None,
            hidden: false,
            attached_message: None,
        };
        drop(UnicodePstFile::create_with_attachments(
            &path,
            &message,
            &[inline_attachment, attachment, empty_attachment],
        )?);
        let subjects = (1..10)
            .map(|index| format!("Appended message {index}"))
            .collect::<Vec<_>>();
        let message_ids = (1..10)
            .map(|index| format!("<append-{index}@example.com>"))
            .collect::<Vec<_>>();
        let messages = subjects
            .iter()
            .zip(&message_ids)
            .map(|(subject, message_id)| UnicodePstMessage {
                subject,
                message_id: Some(message_id),
                body: "Incrementally appended body.",
                ..message
            })
            .collect::<Vec<_>>();
        drop(UnicodePstFile::append_many(&path, &messages)?);
    } else {
        drop(UnicodePstFile::create_with_attachments(
            &path,
            &message,
            &[inline_attachment],
        )?);
    }
    drop(UnicodePstFile::append_in_folder(
        &path,
        &["Imported", "2026"],
        &UnicodePstMessage {
            subject: "Message in Imported/2026",
            message_id: Some("<memex-folder-message@example.com>"),
            ..message
        },
    )?);
    println!("{}", path.display());
    Ok(())
}
