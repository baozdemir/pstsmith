use pstsmith::convert::{convert_manifest, ManifestOptions, Output};
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pstsmith-convert-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const HEADER_ONE_ITEM: &str = r#"{"schema": "pst-export-manifest/2", "export_id": "one", "item_count": 1, "folder_count": 1}"#;
const HEADER_THREE_ITEMS: &str = r#"{"schema": "pst-export-manifest/2", "export_id": "three", "item_count": 3, "folder_count": 1}"#;
const INBOX: &str = r#"{"folder_path": ["Inbox"]}"#;

fn item(key: &str, search_key: &str) -> String {
    format!(
        r#"{{"item_key": "{key}", "search_key": "{search_key}", "message_id": "<{key}@example.invalid>", "folder_path": ["Inbox"], "subject": "Günaydın {key}", "sender": {{"name": "Sender", "email": "sender@example.invalid"}}, "to": [{{"name": "To", "email": "to@example.invalid", "properties": []}}], "cc": [], "bcc": [], "body": "Body", "html": null, "sent_time": 1704067144, "received_time": 1704067204, "is_read": true, "importance": 1, "categories": [], "attachments": [], "message_class": "IPM.Note", "transport_headers": null, "created_time": 1704067204, "last_modified_time": 1704067204, "is_draft": false, "properties": []}}"#
    )
}

fn write_manifest(dir: &PathBuf, lines: &[&str]) -> PathBuf {
    let path = dir.join("manifest.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

#[test]
fn converts_a_manifest_and_reports_it() {
    let dir = temp_dir("one");
    let one = item("one-000001", "d84d31617bc154cf80e289d2dd4a62b9");
    let manifest = write_manifest(&dir, &[HEADER_ONE_ITEM, INBOX, &one]);
    let output = dir.join("out.pst");
    let report = convert_manifest(
        &manifest,
        &Output::File(output.clone()),
        &ManifestOptions::default(),
    )
    .unwrap();
    assert_eq!(report.written(), 1);
    assert_eq!(report.skipped(), 0);
    assert_eq!(report.outputs(), vec![output.clone()]);
    let line: serde_json::Value = serde_json::from_str(&report.result_line()).unwrap();
    assert_eq!(line["protocol"], "pst-export-result/1");
    assert_eq!(line["engine"]["name"], "pstsmith");
    assert!(output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn refuses_an_existing_output() {
    let dir = temp_dir("exists");
    let one = item("one-000001", "d84d31617bc154cf80e289d2dd4a62b9");
    let manifest = write_manifest(&dir, &[HEADER_ONE_ITEM, INBOX, &one]);
    let output = dir.join("out.pst");
    std::fs::write(&output, b"keep").unwrap();
    let error = convert_manifest(
        &manifest,
        &Output::File(output.clone()),
        &ManifestOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, "output-exists");
    assert_eq!(error.exit_code, 4);
    assert_eq!(std::fs::read(&output).unwrap(), b"keep");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_failure_after_a_commit_removes_the_output() {
    let dir = temp_dir("failure");
    // With one item per commit, the first item is committed when the second is read; the third is invalid.
    let first = item("three-000001", "d84d31617bc154cf80e289d2dd4a62b9");
    let second = item("three-000002", "e84d31617bc154cf80e289d2dd4a62b9");
    let third = item("three-000003", "not-hex");
    let manifest = write_manifest(&dir, &[HEADER_THREE_ITEMS, INBOX, &first, &second, &third]);
    let output = dir.join("out.pst");
    let options = ManifestOptions {
        batch_items: 1,
        ..ManifestOptions::default()
    };
    let error = convert_manifest(&manifest, &Output::File(output.clone()), &options).unwrap_err();
    assert_eq!(error.code, "invalid-manifest");
    assert_eq!(error.exit_code, 2);
    assert_eq!(error.item_key.as_deref(), Some("three-000003"));
    assert!(
        !output.exists(),
        "a failed conversion leaves no partial output"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_unconverted_class_is_named_in_the_error() {
    let dir = temp_dir("class");
    let contact = item("one-000001", "d84d31617bc154cf80e289d2dd4a62b9")
        .replace("\"IPM.Note\"", "\"IPM.Contact\"");
    let manifest = write_manifest(&dir, &[HEADER_ONE_ITEM, INBOX, &contact]);
    let error = convert_manifest(
        &manifest,
        &Output::File(dir.join("out.pst")),
        &ManifestOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, "unsupported-item-class");
    assert!(error.detail.contains("\"IPM.Contact\""), "{}", error.detail);
    assert!(
        error
            .detail
            .ends_with("(calendar items, contacts, tasks and other classes are not converted)"),
        "{}",
        error.detail
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
