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

/// A conversion stopped by SIGTERM, SIGINT (Ctrl-C) or SIGHUP removes what it wrote: the staging directory, which holds
/// decoded mail, and the partial output, which would look like a complete PST with fewer messages.
#[cfg(unix)]
#[test]
fn a_stopped_convert_removes_its_staging_and_output() {
    use std::io::Write;
    let dir = temp_dir("convert-stopped");
    let mbox = dir.join("Big.mbox");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&mbox).unwrap());
    for index in 0..60_000 {
        write!(file, "From a Mon Jan  1 10:00:00 2024\nFrom: a@example.invalid\nSubject: message {index}\n\nbody {index}\n\n").unwrap();
    }
    drop(file);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    for signal in ["TERM", "INT", "HUP"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
            .args(["convert", mbox.to_str().unwrap(), "-o", output.to_str().unwrap(), "--temp-dir", staging.to_str().unwrap()])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Wait until the conversion has staged some mail, then stop it.
        let started = std::time::Instant::now();
        while std::fs::read_dir(&staging).unwrap().next().is_none() {
            assert!(started.elapsed() < std::time::Duration::from_secs(30), "no staging directory appeared");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(Command::new("kill").args([&format!("-{signal}"), &child.id().to_string()]).status().unwrap().success());
        let result = child.wait_with_output().unwrap();
        assert!(!result.status.success(), "SIG{signal}: the conversion finished before the signal; make the input larger");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.lines().next().is_some_and(|line| line.contains("\"error\":\"interrupted\"")), "SIG{signal}: {stderr}");
        let left: Vec<_> = std::fs::read_dir(&staging).unwrap().map(|entry| entry.unwrap().path()).collect();
        assert!(left.is_empty(), "SIG{signal}: staging left behind: {left:?}");
        assert!(!output.exists(), "SIG{signal}: a partial output was left behind");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
