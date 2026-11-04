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
