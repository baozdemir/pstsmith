use std::process::Command;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pstsmith-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn result_line_names_pstsmith() {
    let dir = temp_dir("result-line");
    let manifest = dir.join("folders.jsonl");
    std::fs::write(
        &manifest,
        concat!(
            r#"{"schema": "pst-export-manifest/2", "export_id": "folders", "item_count": 0, "folder_count": 1}"#,
            "\n",
            r#"{"folder_path": ["Inbox"]}"#,
            "\n"
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
        .args(["--manifest", manifest.to_str().unwrap(), "--output", dir.join("out.pst").to_str().unwrap()])
        .args(["--excluded", dir.join("excluded.jsonl").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(line["engine"]["name"], "pstsmith");
    assert_eq!(line["engine"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(line["engine"]["writer"], format!("pstsmith {}", env!("CARGO_PKG_VERSION")));
    std::fs::remove_dir_all(&dir).unwrap();
}

const MBOX: &str = "From a Mon Jan  1 10:00:00 2024\nFrom: a@example.invalid\nSubject: one\nDate: Mon, 01 Jan 2024 10:00:00 +0000\n\nfirst\n\nFrom a Mon Jan  1 11:00:00 2024\nFrom: a@example.invalid\nSubject: two\n\nsecond\n";

#[test]
fn cli_convert_json_matches_report() {
    let dir = temp_dir("convert-json");
    let mbox = dir.join("Inbox.mbox");
    std::fs::write(&mbox, MBOX).unwrap();
    let output = dir.join("out.pst");
    let run = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
        .args(["convert", mbox.to_str().unwrap(), "-o", output.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let line: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(line["protocol"], "pst-export-result/1");
    assert_eq!(line["written"], 2);
    assert_eq!(line["output"]["path"], output.to_str().unwrap());
    let summary = String::from_utf8_lossy(&run.stderr);
    assert!(summary.contains("2 messages"), "{summary}");
    assert!(output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_convert_without_json_prints_only_the_summary() {
    let dir = temp_dir("convert-human");
    let mbox = dir.join("Inbox.mbox");
    std::fs::write(&mbox, MBOX).unwrap();
    let output = dir.join("out.pst");
    let run = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
        .args(["convert", mbox.to_str().unwrap(), "--output", output.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert!(run.stdout.is_empty());
    assert!(String::from_utf8_lossy(&run.stderr).contains(output.to_str().unwrap()));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_convert_refuses_a_missing_input() {
    let dir = temp_dir("convert-missing");
    let run = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
        .args(["convert", dir.join("nope.mbox").to_str().unwrap(), "-o", dir.join("out.pst").to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    let line: serde_json::Value = serde_json::from_slice(run.stderr.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(line["error"], "input-unreadable");
    assert!(!dir.join("out.pst").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_help_and_version() {
    let help = Command::new(env!("CARGO_BIN_EXE_pstsmith")).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("pstsmith convert") && text.contains("--manifest"), "{text}");
    let version = Command::new(env!("CARGO_BIN_EXE_pstsmith")).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), format!("pstsmith {}", env!("CARGO_PKG_VERSION")));
}
