//! EML, MBOX, Maildir and Thunderbird input converted to PST files and read back.

use pstsmith::convert::mail::Converter;
use pstsmith::convert::{ManifestOptions, Output};
use pstsmith::ltp::prop_context::PropertyValue;
use pstsmith::messaging::store::Store;
use pstsmith::ndb::node_id::NodeId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pstsmith-mail-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn eml(subject: &str) -> String {
    format!(
        "From: Sender <sender@example.invalid>\r\nTo: to@example.invalid\r\nSubject: {subject}\r\n\
         Date: Mon, 01 Jan 2024 10:00:00 +0000\r\nMessage-ID: <{}@example.invalid>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nBody of {subject}\r\n",
        subject.replace(' ', "-")
    )
}

/// Every folder under the IPM subtree, by path, with the subjects of its messages in contents-table order.
fn tree(path: &Path) -> BTreeMap<Vec<String>, Vec<String>> {
    let store = pstsmith::open_store(path).unwrap();
    let root = store.properties().ipm_sub_tree_entry_id().unwrap().node_id();
    let mut out = BTreeMap::new();
    walk(&store, root, Vec::new(), &mut out);
    out
}

fn walk(store: &Rc<dyn Store>, folder: NodeId, path: Vec<String>, out: &mut BTreeMap<Vec<String>, Vec<String>>) {
    let opened = store.open_folder(&store.properties().make_entry_id(folder).unwrap()).unwrap();
    if !path.is_empty() {
        let mut subjects = Vec::new();
        if let Some(contents) = opened.contents_table() {
            for row in contents.rows_matrix() {
                let id = NodeId::from(u32::from(row.id()));
                let message = store.open_message(&store.properties().make_entry_id(id).unwrap(), None).unwrap();
                match message.properties().get(0x0037) {
                    Some(PropertyValue::Unicode(value)) => subjects.push(value.to_string()),
                    _ => subjects.push(String::new()),
                }
            }
        }
        out.insert(path.clone(), subjects);
    }
    if let Some(hierarchy) = opened.hierarchy_table() {
        for row in hierarchy.rows_matrix() {
            let child = NodeId::from(u32::from(row.id()));
            let name = store.open_folder(&store.properties().make_entry_id(child).unwrap()).unwrap().properties().display_name().unwrap();
            let mut child_path = path.clone();
            child_path.push(name);
            walk(store, child, child_path, out);
        }
    }
}

/// The plain text body of the first message in `folder`.
fn first_body(path: &Path, folder: &[&str]) -> String {
    let store = pstsmith::open_store(path).unwrap();
    let mut node = store.properties().ipm_sub_tree_entry_id().unwrap().node_id();
    for name in folder {
        let opened = store.open_folder(&store.properties().make_entry_id(node).unwrap()).unwrap();
        let hierarchy = opened.hierarchy_table().unwrap();
        node = hierarchy
            .rows_matrix()
            .map(|row| NodeId::from(u32::from(row.id())))
            .find(|child| store.open_folder(&store.properties().make_entry_id(*child).unwrap()).unwrap().properties().display_name().unwrap() == *name)
            .unwrap();
    }
    let opened = store.open_folder(&store.properties().make_entry_id(node).unwrap()).unwrap();
    let row = opened.contents_table().unwrap().rows_matrix().next().map(|row| NodeId::from(u32::from(row.id()))).unwrap();
    let message = store.open_message(&store.properties().make_entry_id(row).unwrap(), None).unwrap();
    match message.properties().get(0x1000) {
        Some(PropertyValue::Unicode(value)) => value.to_string(),
        other => panic!("no body: {other:?}"),
    }
}

fn convert(inputs: &[&Path], output: &Path) -> pstsmith::convert::Report {
    let mut converter = Converter::create(Output::File(output.to_path_buf()), &ManifestOptions::default()).unwrap();
    for input in inputs {
        converter.add_path(input).unwrap();
    }
    converter.finish().unwrap()
}

fn key(path: &[&str]) -> Vec<String> {
    path.iter().map(|name| name.to_string()).collect()
}

#[test]
fn mbox_mboxrd_unquotes_from_lines() {
    let dir = temp_dir("mboxrd");
    let mbox = dir.join("Inbox.mbox");
    let first = eml("first").replace("Body of first\r\n", ">From the start\r\n>>From quoted twice\r\n");
    write(&mbox, format!("From sender@example.invalid Mon Jan  1 10:00:00 2024\r\n{first}\r\nFrom sender@example.invalid Mon Jan  1 11:00:00 2024\r\n{}", eml("second")).as_bytes());
    let output = dir.join("out.pst");
    let report = convert(&[&mbox], &output);
    assert_eq!(report.written(), 2);
    assert_eq!(tree(&output)[&key(&["Inbox"])], vec!["first", "second"]);
    assert_eq!(first_body(&output, &["Inbox"]), "From the start\r\n>From quoted twice\r\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mbox_crlf_and_lf_give_the_same_messages() {
    let dir = temp_dir("crlf");
    let lf = dir.join("lf/Mail.mbox");
    let crlf = dir.join("crlf/Mail.mbox");
    let body = format!("From a Mon Jan  1 10:00:00 2024\n{}\nFrom a Mon Jan  1 11:00:00 2024\n{}", eml("one"), eml("two"));
    write(&lf, body.replace("\r\n", "\n").as_bytes());
    write(&crlf, body.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    let a = dir.join("a.pst");
    let b = dir.join("b.pst");
    convert(&[&lf], &a);
    convert(&[&crlf], &b);
    assert_eq!(tree(&a), tree(&b));
    assert_eq!(tree(&a)[&key(&["Mail"])], vec!["one", "two"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn thunderbird_sbd_tree() {
    let dir = temp_dir("thunderbird");
    let profile = dir.join("Local Folders");
    write(&profile.join("Inbox"), format!("From - Mon Jan  1 10:00:00 2024\r\nX-Mozilla-Status: 0001\r\n{}", eml("in inbox")).as_bytes());
    write(&profile.join("Inbox.msf"), b"// index, not mail");
    write(&profile.join("Inbox.sbd/Work"), format!("From - Mon Jan  1 10:00:00 2024\r\n{}", eml("at work")).as_bytes());
    write(
        &profile.join("Trash"),
        format!("From - Mon Jan  1 10:00:00 2024\r\nX-Mozilla-Status: 0009\r\n{}", eml("expunged")).as_bytes(),
    );
    let output = dir.join("out.pst");
    let report = convert(&[&profile], &output);
    let folders = tree(&output);
    assert_eq!(folders[&key(&["Inbox"])], vec!["in inbox"]);
    assert_eq!(folders[&key(&["Inbox", "Work"])], vec!["at work"]);
    // Thunderbird marks deleted messages and removes them only when the folder is compacted.
    assert_eq!(folders[&key(&["Trash"])], Vec::<String>::new());
    assert_eq!(report.written(), 2);
    assert!(report.warnings().iter().any(|warning| warning.contains("deleted in Thunderbird")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apple_mail_mbox_directory() {
    let dir = temp_dir("apple");
    let export = dir.join("Export");
    write(&export.join("Clients.mbox/mbox"), format!("From a Mon Jan  1 10:00:00 2024\n{}", eml("apple")).as_bytes());
    write(&export.join("Clients.mbox/Info.plist"), b"<plist/>");
    let output = dir.join("out.pst");
    convert(&[&export], &output);
    assert_eq!(tree(&output)[&key(&["Clients"])], vec!["apple"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn maildir_flags_and_plus_names() {
    let dir = temp_dir("maildir");
    let root = dir.join("Maildir");
    // Windows file names cannot hold ':'; Maildir++ writes "!2," there, and both are read.
    let info = if cfg!(windows) { "!2," } else { ":2," };
    write(&root.join(format!("cur/1700000001.1.host{info}S")), eml("seen").as_bytes());
    write(&root.join("new/1700000002.2.host"), eml("new").as_bytes());
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    write(&root.join(format!(".Sent/cur/1700000003.3.host{info}S")), eml("sent").as_bytes());
    std::fs::create_dir_all(root.join(".Sent/new")).unwrap();
    write(&root.join(format!(".Archive.2024/cur/1700000004.4.host{info}DS")), eml("archived").as_bytes());
    std::fs::create_dir_all(root.join(".Archive.2024/new")).unwrap();
    let output = dir.join("out.pst");
    let report = convert(&[&root], &output);
    let folders = tree(&output);
    assert_eq!(folders[&key(&["Inbox"])].len(), 2);
    assert_eq!(folders[&key(&["Sent"])], vec!["sent"]);
    assert_eq!(folders[&key(&["Archive", "2024"])], vec!["archived"]);
    assert_eq!(report.written(), 4);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn case_only_folder_names_warn() {
    let dir = temp_dir("case");
    let output = dir.join("out.pst");
    let mut converter = Converter::create(Output::File(output.clone()), &ManifestOptions::default()).unwrap();
    converter.add_eml(&["Inbox"], eml("upper").as_bytes()).unwrap();
    converter.add_eml(&["inbox"], eml("lower").as_bytes()).unwrap();
    let report = converter.finish().unwrap();
    let folders = tree(&output);
    assert_eq!(folders[&key(&["Inbox"])], vec!["upper"]);
    assert_eq!(folders[&key(&["inbox"])], vec!["lower"]);
    assert!(report.warnings().iter().any(|warning| warning.contains("differ only in case")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn eml_directory_tree_becomes_folders() {
    let dir = temp_dir("emltree");
    let export = dir.join("Export");
    write(&export.join("top.eml"), eml("at the top").as_bytes());
    write(&export.join("Projects/2026/plan.eml"), eml("plan").as_bytes());
    write(&export.join("Projects/notes.txt"), b"not mail");
    let output = dir.join("out.pst");
    let report = convert(&[&export], &output);
    let folders = tree(&output);
    assert_eq!(folders[&key(&["Export"])], vec!["at the top"]);
    assert_eq!(folders[&key(&["Projects", "2026"])], vec!["plan"]);
    assert_eq!(folders[&key(&["Projects"])], Vec::<String>::new());
    assert_eq!(report.written(), 2);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn single_eml_file_and_add_eml() {
    let dir = temp_dir("single");
    let file = dir.join("Saved/one.eml");
    write(&file, eml("one").as_bytes());
    let output = dir.join("out.pst");
    let mut converter = Converter::create(Output::File(output.clone()), &ManifestOptions::default()).unwrap();
    converter.add_path(&file).unwrap();
    converter.add_eml(&["Imported", "Batch 1"], eml("from memory").as_bytes()).unwrap();
    let report = converter.finish().unwrap();
    assert_eq!(report.written(), 2);
    let folders = tree(&output);
    assert_eq!(folders[&key(&["Saved"])], vec!["one"]);
    assert_eq!(folders[&key(&["Imported", "Batch 1"])], vec!["from memory"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn same_input_same_bytes_after_move() {
    let dir = temp_dir("move");
    let mbox = |root: &Path| {
        let path = root.join("Mail/Inbox.mbox");
        write(&path, format!("From a Mon Jan  1 10:00:00 2024\n{}\nFrom a Mon Jan  1 11:00:00 2024\n{}", eml("one"), eml("two")).as_bytes());
        path
    };
    let a = mbox(&dir.join("first place"));
    let b = mbox(&dir.join("second/place"));
    // The store's display name is the output file's name, so both outputs are named alike.
    std::fs::create_dir_all(dir.join("out-a")).unwrap();
    std::fs::create_dir_all(dir.join("out-b")).unwrap();
    let out_a = dir.join("out-a/archive.pst");
    let out_b = dir.join("out-b/archive.pst");
    std::env::set_current_dir(a.parent().unwrap().parent().unwrap()).unwrap();
    convert(&[Path::new("Mail/Inbox.mbox")], &out_a);
    std::env::set_current_dir(b.parent().unwrap().parent().unwrap()).unwrap();
    convert(&[Path::new("Mail/Inbox.mbox")], &out_b);
    assert_eq!(std::fs::read(&out_a).unwrap(), std::fs::read(&out_b).unwrap());
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn output_removed_on_failure() {
    let dir = temp_dir("failure");
    let output = dir.join("out.pst");
    let mut converter = Converter::create(Output::File(output.clone()), &ManifestOptions::default()).unwrap();
    converter.add_eml(&["Inbox"], eml("fine").as_bytes()).unwrap();
    let missing = converter.add_path(&dir.join("does-not-exist.mbox")).unwrap_err();
    assert_eq!(missing.exit_code, 2);
    let error = Converter::create(Output::File(output.clone()), &ManifestOptions::default()).map(|_| ());
    assert!(error.is_ok(), "nothing was written before finish");
    std::fs::write(&output, b"keep").unwrap();
    let exists = Converter::create(Output::File(output.clone()), &ManifestOptions::default()).unwrap_err();
    assert_eq!(exists.code, "output-exists");
    assert_eq!(std::fs::read(&output).unwrap(), b"keep");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn staging_is_removed_after_finish() {
    let dir = temp_dir("staging");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let mut converter =
        Converter::create_in(Output::File(dir.join("out.pst")), &ManifestOptions::default(), &staging).unwrap();
    converter.add_eml(&["Inbox"], eml("staged").as_bytes()).unwrap();
    converter.finish().unwrap();
    assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A 2 GB MBOX with one 300 MB message: the converter reads it as a stream. Run with `--ignored`; the peak RSS is
/// checked from outside the test (`/usr/bin/time`), this test only proves the conversion completes and is complete.
#[test]
#[ignore]
fn mbox_streams_without_reading_whole_file() {
    use std::io::Write;
    let dir = temp_dir("stream");
    let mbox = dir.join("Big.mbox");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&mbox).unwrap());
    let line = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3\r\n";
    let mut written = 0u64;
    let mut count = 0u64;
    let big = 300u64 << 20;
    while written < (2u64 << 30) {
        count += 1;
        let size = if count == 1 { big } else { 4 << 20 };
        let header = format!(
            "From a Mon Jan  1 10:00:00 2024\r\nFrom: a@example.invalid\r\nSubject: part {count}\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nbody\r\n--b\r\n\
             Content-Type: application/octet-stream; name=data.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        );
        file.write_all(header.as_bytes()).unwrap();
        let mut body = 0u64;
        while body < size {
            file.write_all(line.as_bytes()).unwrap();
            body += line.len() as u64;
        }
        file.write_all(b"--b--\r\n\r\n").unwrap();
        written += header.len() as u64 + body;
    }
    file.flush().unwrap();
    drop(file);
    let output = dir.join("out.pst");
    let report = convert(&[&mbox], &output);
    assert_eq!(report.written(), count);
    std::fs::remove_dir_all(&dir).unwrap();
}

