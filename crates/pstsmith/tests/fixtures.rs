//! Every fixture under `tests/fixtures` converted, and each output's SHA-256 compared with `expected.sha256`: the same
//! input gives the same bytes on every platform. `PSTSMITH_BLESS=1` rewrites the list instead (after a deliberate
//! change of output, reviewed in the diff). Run `python3 tests/fixtures/generate.py` first: it writes the 64 MiB
//! attachment that three manifest fixtures name.

use pstsmith::convert::mail::Converter;
use pstsmith::convert::{convert_manifest, ManifestOptions, Output};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The fixtures directory, without `canonicalize`: on Windows that gives a `\\?\` path, which stops being one once
/// its separators are written as `/` in a manifest.
fn fixtures() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate_dir.parent().and_then(Path::parent).expect("crates/pstsmith is two levels below the repository").join("tests/fixtures")
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

/// The cases and their mail inputs: a name (the output's file name, and so its store's display name) and the inputs.
const MAIL: &[(&str, &str)] = &[
    ("mail-eml", "mail/eml"),
    ("mail-takeout", "mail/takeout/All mail Including Spam and Trash.mbox"),
    ("mail-thunderbird", "mail/thunderbird/Local Folders"),
    ("mail-apple", "mail/apple/Export"),
    ("mail-apple-mailbox", "mail/apple/Export/Clients.mbox"),
    ("mail-crlf", "mail/crlf/Windows.mbox"),
    ("mail-maildir", "mail/maildir/Maildir"),
];

#[test]
fn fixtures_give_the_expected_bytes() {
    let root = fixtures();
    let large = root.join("manifest/files/large-64MiB.bin");
    assert!(large.exists(), "run `python3 tests/fixtures/generate.py` first: {} is missing", large.display());
    let out = std::env::temp_dir().join(format!("pstsmith-fixtures-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let mut hashes = BTreeMap::new();
    // Manifest fixtures: the attachment paths are written as @FIXTURES@/files/..., resolved here.
    let files = root.join("manifest").to_string_lossy().replace('\\', "/");
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(root.join("manifest")).unwrap().map(|entry| entry.unwrap().path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect();
    manifests.sort();
    for manifest in manifests {
        let name = manifest.file_stem().unwrap().to_string_lossy().into_owned();
        let resolved = out.join(format!("{name}.manifest.jsonl"));
        std::fs::write(&resolved, std::fs::read_to_string(&manifest).unwrap().replace("@FIXTURES@", &files)).unwrap();
        let pst = out.join(format!("{name}.pst"));
        let excluded = out.join(format!("{name}.excluded.jsonl"));
        let options = ManifestOptions { excluded: name.starts_with("v2-").then(|| excluded.clone()), ..ManifestOptions::default() };
        convert_manifest(&resolved, &Output::File(pst.clone()), &options).unwrap_or_else(|error| panic!("{name}: {error}"));
        hashes.insert(format!("manifest/{name}.pst"), sha256(&pst));
        if excluded.exists() {
            hashes.insert(format!("manifest/{name}.excluded.jsonl"), sha256(&excluded));
        }
    }
    for (name, input) in MAIL {
        let pst = out.join(format!("{name}.pst"));
        let mut converter = Converter::create(Output::File(pst.clone()), &ManifestOptions::default()).unwrap();
        converter.add_path(&root.join(input)).unwrap_or_else(|error| panic!("{name}: {error}"));
        converter.finish().unwrap_or_else(|error| panic!("{name}: {error}"));
        hashes.insert(format!("{name}.pst"), sha256(&pst));
    }
    let listed: String = hashes.iter().map(|(name, hash)| format!("{hash}  {name}\n")).collect();
    let expected_path = root.join("expected.sha256");
    if std::env::var("PSTSMITH_BLESS").as_deref() == Ok("1") {
        std::fs::write(&expected_path, &listed).unwrap();
    } else {
        let expected = std::fs::read_to_string(&expected_path).unwrap_or_default();
        assert_eq!(listed, expected, "outputs differ from tests/fixtures/expected.sha256 (outputs kept in {})", out.display());
    }
    std::fs::remove_dir_all(&out).unwrap();
}
