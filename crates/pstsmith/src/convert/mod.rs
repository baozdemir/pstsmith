//! Conversion of mail into PST files: the pipeline behind the `pstsmith` command line.
//!
//! [`convert_manifest`] reads a JSONL manifest of mail items (`pst-export-manifest/1` or `/2`, documented in
//! `docs/MANIFEST.md`) and writes one PST file or size-limited parts. A failed conversion removes everything it
//! created, so a caller never sees a partial output.

mod manifest;

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The file size limits the command line accepts for `--max-file-size`, in bytes.
pub const FILE_SIZE_CHOICES: &[(&str, u64)] = manifest::FILE_SIZE_CHOICES;

/// Where a conversion writes. The path must not exist yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// One PST file.
    File(PathBuf),
    /// A new directory of size-limited parts, `export-0001.pst`, `export-0002.pst` and so on (manifest v2 only).
    Directory(PathBuf),
}

/// How a manifest is converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestOptions {
    /// The largest file a commit may leave, in bytes (manifest v2 only; `None` is 10 GB, the command line's default).
    pub max_file_size: Option<u64>,
    /// Where items that cannot be written faithfully are recorded, one JSON line each, instead of failing the
    /// conversion (manifest v2 only). The path must not exist yet.
    pub excluded: Option<PathBuf>,
    /// Items per commit, at least 1.
    pub batch_items: usize,
    /// Bytes of bodies and HTML held per commit, at least 1 (attachments are streamed and do not count).
    pub batch_bytes: usize,
}

impl Default for ManifestOptions {
    /// 4,096 items and 64 MiB per commit. A commit does not re-read the file, so its cost does not grow with the file;
    /// fewer commits mean fewer sync barriers (three per commit).
    fn default() -> Self {
        Self { max_file_size: None, excluded: None, batch_items: 4096, batch_bytes: 64 * 1024 * 1024 }
    }
}

/// A finished conversion: the `pst-export-result/1` report.
#[derive(Clone, Debug)]
pub struct Report {
    line: serde_json::Value,
}

impl Report {
    /// The report as the one JSON line the command line prints on stdout.
    pub fn result_line(&self) -> String {
        self.line.to_string()
    }

    /// Items written.
    pub fn written(&self) -> u64 {
        self.line["written"].as_u64().unwrap_or(0)
    }

    /// Items left out by name (recorded in the exclusion record).
    pub fn skipped(&self) -> u64 {
        self.line["skipped"].as_u64().unwrap_or(0)
    }

    /// The PST files written, in order.
    pub fn outputs(&self) -> Vec<PathBuf> {
        match self.line.get("outputs").and_then(|outputs| outputs.as_array()) {
            Some(outputs) => outputs.iter().filter_map(|output| output["path"].as_str()).map(PathBuf::from).collect(),
            None => self.line["output"]["path"].as_str().map(PathBuf::from).into_iter().collect(),
        }
    }
}

/// Why a conversion failed. Carries no message content: no subjects, bodies, recipient names or file names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvertError {
    /// The error code, for example `invalid-manifest`, `output-exists`, `source-mismatch` or `writer`.
    pub code: &'static str,
    /// The command line's exit code for it: 2 invalid input, 3 an attachment that does not match its recorded size or
    /// hash, 4 an output path that exists, 5 a writer or I/O failure.
    pub exit_code: u8,
    /// The item concerned, if the error belongs to one item.
    pub item_key: Option<String>,
    /// A short description.
    pub detail: String,
}

impl ConvertError {
    /// The error as the one JSON line the command line prints on stderr: `{"error", "item_key", "detail"}`.
    pub fn error_line(&self) -> String {
        serde_json::json!({ "error": self.code, "item_key": self.item_key, "detail": self.detail }).to_string()
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ConvertError {}

impl From<manifest::Failure> for ConvertError {
    fn from(failure: manifest::Failure) -> Self {
        Self { code: failure.code, exit_code: failure.exit, item_key: failure.item_key, detail: failure.detail }
    }
}

/// Converts the manifest at `manifest` into `output`.
///
/// # Errors
///
/// A [`ConvertError`] for invalid input, an existing output path, an attachment that does not match its recorded size
/// or hash, or a writer or I/O failure. Everything this call created is removed first.
pub fn convert_manifest(manifest: &Path, output: &Output, options: &ManifestOptions) -> Result<Report, ConvertError> {
    if options.batch_items == 0 || options.batch_bytes == 0 {
        return Err(ConvertError { code: "usage", exit_code: 2, item_key: None, detail: "batch limits must be positive".into() });
    }
    let options = manifest::Options {
        manifest: manifest.to_path_buf(),
        output: match output {
            Output::File(path) => manifest::Target::File(path.clone()),
            Output::Directory(path) => manifest::Target::Directory(path.clone()),
        },
        file_size: options.max_file_size,
        excluded: options.excluded.clone(),
        batch_items: options.batch_items,
        batch_bytes: options.batch_bytes,
    };
    Ok(Report { line: manifest::run(&options)? })
}
