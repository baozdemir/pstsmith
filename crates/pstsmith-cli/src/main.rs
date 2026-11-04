//! The pstsmith command line: converts a JSONL manifest of mail items into one PST file or size-limited parts.
//!
//! Input: a `pst-export-manifest/1` JSONL file (header line + one item per line), or a `pst-export-manifest/2` file
//! (header, `folder_count` folder records, then the items).
//! Output: one new PST at `--output`, or (manifest v2 only) a new directory at `--output-dir` holding size-limited
//! parts (`--max-file-size`), `export-0001.pst`, `export-0002.pst` and so on; and a single `pst-export-result/1` JSON
//! line on stdout. With `--excluded PATH` (manifest v2 only), an item the converter cannot write faithfully (a message
//! class it does not convert, or attached e-mails nested deeper than the writer holds) is left out by name and recorded
//! in that new JSONL file, and the rest converts; without it such an item fails the conversion.
//! Errors: a single JSON line on stderr carrying a code, the item key and a short detail. No
//! subjects, bodies, recipient names or file names are written to stderr.
//!
//! Exit codes: 0 converted; 2 invalid usage/manifest (including an empty selection, and an item the PST format cannot
//! hold: `item-too-large`); 3 source attachment does not match its recorded size/sha256; 4 output path already exists;
//! 5 writer or I/O failure. On any failure after the output was created, the converter removes what this attempt
//! created: the output files, their directory and the exclusion record.
#[cfg(feature = "mem-profile")]
#[global_allocator]
static COUNTING: pstsmith::measurement::heap::Counting = pstsmith::measurement::heap::Counting;

use pstsmith::convert::{convert_manifest, ConvertError, ManifestOptions, Output, FILE_SIZE_CHOICES};
use std::{env, path::PathBuf, process::ExitCode};

struct Options {
    manifest: PathBuf,
    output: Output,
    manifest_options: ManifestOptions,
}

fn usage(detail: impl Into<String>) -> ConvertError {
    ConvertError { code: "usage", exit_code: 2, item_key: None, detail: detail.into() }
}

fn parse_args() -> Result<Options, ConvertError> {
    let mut manifest = None;
    let mut output = None;
    let mut output_dir = None;
    let mut file_size = None;
    let mut excluded = None;
    let ManifestOptions { mut batch_items, mut batch_bytes, .. } = ManifestOptions::default();
    let mut args = env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| usage(format!("missing value for {flag}")))?;
        match flag.as_str() {
            "--manifest" => manifest = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            "--output-dir" => output_dir = Some(PathBuf::from(value)),
            "--max-file-size" => {
                let size = FILE_SIZE_CHOICES.iter().find(|(name, _)| *name == value).map(|(_, size)| *size);
                file_size = Some(size.ok_or_else(|| usage("--max-file-size must be 1GB, 2GB, 5GB or 10GB"))?);
            }
            #[cfg(feature = "test-file-sizes")]
            "--max-file-bytes" => file_size = Some(value.parse().ok().filter(|size| *size > 0).ok_or_else(|| usage("--max-file-bytes must be a positive integer"))?),
            "--excluded" => excluded = Some(PathBuf::from(value)),
            "--batch" => batch_items = value.parse().map_err(|_| usage("--batch must be a positive integer"))?,
            "--batch-bytes" => batch_bytes = value.parse().map_err(|_| usage("--batch-bytes must be a positive integer"))?,
            other => return Err(usage(format!("unknown argument {other}"))),
        }
    }
    if batch_items == 0 || batch_bytes == 0 {
        return Err(usage("batch limits must be positive"));
    }
    let output = match (output, output_dir) {
        (Some(path), None) => Some(Output::File(path)),
        (None, Some(path)) => Some(Output::Directory(path)),
        _ => None,
    };
    match (manifest, output) {
        (Some(manifest), Some(output)) => {
            Ok(Options { manifest, output, manifest_options: ManifestOptions { max_file_size: file_size, excluded, batch_items, batch_bytes } })
        }
        _ => Err(usage("usage: pstsmith --manifest items.jsonl (--output export.pst | --output-dir directory) [--max-file-size 1GB|2GB|5GB|10GB] [--excluded excluded.jsonl] [--batch N] [--batch-bytes N]")),
    }
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => return fail(error),
    };
    match convert_manifest(&options.manifest, &options.output, &options.manifest_options) {
        Ok(report) => {
            println!("{}", report.result_line());
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn fail(error: ConvertError) -> ExitCode {
    eprintln!("{}", error.error_line());
    ExitCode::from(error.exit_code)
}
