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

/// Writes an MBOX large enough that a conversion of it is still running when a test stops it.
#[cfg(unix)]
fn big_mbox(path: &std::path::Path) {
    use std::io::Write;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for index in 0..60_000 {
        write!(file, "From a Mon Jan  1 10:00:00 2024\nFrom: a@example.invalid\nSubject: message {index}\n\nbody {index}\n\n").unwrap();
    }
}

/// Starts `pstsmith convert MBOX -o OUTPUT --temp-dir STAGING` (through `sh -c` with `prefix` before it) and returns it
/// once it has staged some mail.
#[cfg(unix)]
fn start_convert(prefix: &str, mbox: &std::path::Path, output: &std::path::Path, staging: &std::path::Path) -> std::process::Child {
    let script = format!("{prefix} exec \"$0\" convert \"$1\" -o \"$2\" --temp-dir \"$3\"");
    let child = Command::new("sh")
        .args(["-c", &script, env!("CARGO_BIN_EXE_pstsmith"), mbox.to_str().unwrap(), output.to_str().unwrap(), staging.to_str().unwrap()])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while std::fs::read_dir(staging).unwrap().next().is_none() {
        assert!(started.elapsed() < std::time::Duration::from_secs(30), "no staging directory appeared");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    child
}

#[cfg(unix)]
fn send(child: &std::process::Child, signal: &str) {
    assert!(Command::new("kill").args([&format!("-{signal}"), &child.id().to_string()]).status().unwrap().success());
}

/// A conversion stopped by SIGTERM, SIGINT (Ctrl-C), SIGQUIT or SIGHUP removes what it created: the staging
/// directory, which holds decoded mail, and the partial output, which would look like a complete PST with fewer
/// messages. Then it ends by that signal, so that a shell loop running it stops too.
#[cfg(unix)]
#[test]
fn a_stopped_convert_removes_its_staging_and_output() {
    use std::os::unix::process::ExitStatusExt;
    let dir = temp_dir("convert-stopped");
    let mbox = dir.join("Big.mbox");
    big_mbox(&mbox);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    for (signal, number) in [("TERM", 15), ("INT", 2), ("QUIT", 3), ("HUP", 1)] {
        let child = start_convert("", &mbox, &output, &staging);
        send(&child, signal);
        let result = child.wait_with_output().unwrap();
        if signal == "QUIT" {
            // SIGQUIT's default action dumps core, which would hold decoded mail: it exits with 128 + 3 instead.
            assert_eq!((result.status.code(), result.status.signal()), (Some(131), None), "SIGQUIT: {:?}", result.status);
        } else {
            assert_eq!(result.status.signal(), Some(number), "SIG{signal}: {:?}", result.status);
        }
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.lines().next().is_some_and(|line| line.contains("\"error\":\"interrupted\"")), "SIG{signal}: {stderr}");
        let left: Vec<_> = std::fs::read_dir(&staging).unwrap().map(|entry| entry.unwrap().path()).collect();
        assert!(left.is_empty(), "SIG{signal}: staging left behind: {left:?}");
        assert!(!output.exists(), "SIG{signal}: a partial output was left behind");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A signal ignored when pstsmith starts stays ignored: under `nohup`, closing the terminal does not stop a conversion.
#[cfg(unix)]
#[test]
fn a_signal_ignored_at_start_does_not_stop_convert() {
    let dir = temp_dir("convert-nohup");
    let mbox = dir.join("Big.mbox");
    big_mbox(&mbox);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    let child = start_convert("trap '' HUP;", &mbox, &output, &staging);
    send(&child, "HUP");
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{:?}: {}", result.status, String::from_utf8_lossy(&result.stderr));
    assert!(output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A stopped conversion removes only what it created: a file another program put at its output path stays.
#[cfg(unix)]
#[test]
fn a_stopped_convert_leaves_files_it_did_not_create() {
    let dir = temp_dir("convert-foreign");
    let mbox = dir.join("Big.mbox");
    big_mbox(&mbox);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    let child = start_convert("", &mbox, &output, &staging);
    std::fs::write(&output, b"another program's file").unwrap();
    send(&child, "TERM");
    let result = child.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert_eq!(std::fs::read(&output).unwrap(), b"another program's file");
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Stopped while it writes one large file (the last batch, or a mailbox of fewer messages than a batch holds), a
/// conversion still removes its output: an attachment being written stops the commit.
#[cfg(unix)]
#[test]
fn a_convert_stopped_while_writing_its_file_removes_it() {
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;
    let dir = temp_dir("convert-stopped-writing");
    let mbox = dir.join("Big.mbox");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&mbox).unwrap());
    let line = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3\n";
    for index in 0..40 {
        write!(file, "From a Mon Jan  1 10:00:00 2024\nFrom: a@example.invalid\nSubject: part {index}\nMIME-Version: 1.0\n\
                      Content-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain\n\nbody\n--b\n\
                      Content-Type: application/octet-stream; name=data.bin\nContent-Transfer-Encoding: base64\n\n").unwrap();
        for _ in 0..(6 << 20) / line.len() {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.write_all(b"--b--\n\n").unwrap();
    }
    drop(file);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    let child = Command::new(env!("CARGO_BIN_EXE_pstsmith"))
        .args(["convert", mbox.to_str().unwrap(), "-o", output.to_str().unwrap(), "--temp-dir", staging.to_str().unwrap()])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !output.exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "the output never appeared");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    send(&child, "INT");
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.signal(), Some(2), "{:?}: {}", result.status, String::from_utf8_lossy(&result.stderr));
    assert!(!output.exists(), "the output was left behind");
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// With stderr gone (a closed terminal, a pipe whose reader exited), a stopped conversion still cleans up and ends by
/// the signal instead of panicking on the error line.
#[cfg(unix)]
#[test]
fn a_convert_stopped_with_stderr_closed_still_ends_by_the_signal() {
    use std::os::unix::process::ExitStatusExt;
    let dir = temp_dir("convert-stopped-epipe");
    let mbox = dir.join("Big.mbox");
    big_mbox(&mbox);
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).unwrap();
    let output = dir.join("out.pst");
    let mut child = start_convert("", &mbox, &output, &staging);
    drop(child.stderr.take());
    send(&child, "TERM");
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(15), "{status:?}");
    assert!(!output.exists());
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}
