//! The pstsmith command line.
//!
//! `pstsmith convert INPUT...` converts EML files, MBOX files, Maildir directories and Thunderbird or Apple Mail folders
//! (see [`pstsmith::convert::mail::Converter::add_path`]); a human-readable summary goes to stderr, and with `--json`
//! the `pst-export-result/1` line to stdout. Its warnings name the item concerned by its input path, for the person
//! running the command; the manifest mode below writes no file names.
//!
//! `pstsmith --manifest FILE` converts a JSONL manifest of mail items into one PST file or size-limited parts:
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

use pstsmith::convert::mail::Converter;
use pstsmith::convert::{convert_manifest, ConvertError, ManifestOptions, Output, Report, FILE_SIZE_CHOICES};
use std::{env, path::PathBuf, process::ExitCode};

const HELP: &str = "\
pstsmith: read, create and append PST files without Outlook

Usage:
  pstsmith convert INPUT... (-o FILE.pst | --output-dir DIR) [--max-file-size SIZE] [options]
  pstsmith --manifest ITEMS.jsonl (--output FILE.pst | --output-dir DIR) [manifest options]

convert reads .eml files, MBOX files (Gmail Takeout, Thunderbird, Apple Mail), Maildir directories and
directories of them, and writes one PST file or size-limited parts.

Options:
  -o, --output FILE        write one PST file of at most --max-file-size (it must not exist)
  --output-dir DIR         write parts export-0001.pst, export-0002.pst, ... into a new directory
  --max-file-size SIZE     largest file or part: 1GB, 2GB, 5GB or 10GB (default 10GB, the largest PST
                           Microsoft's own exports write); a larger mailbox needs --output-dir
  --excluded PATH          record items that cannot be written faithfully here instead of failing
  --temp-dir DIR           where convert stages the decoded messages (default: the system temporary directory)
  --json                   print the pst-export-result/1 line on stdout (convert)
  --batch N                items per commit (default 4096)
  --batch-bytes N          body and HTML bytes per commit (default 64 MiB)
  -h, --help               print this help
  -V, --version            print the version

Exit codes: 0 converted; 2 invalid input or usage; 3 an attachment does not match its recorded size or hash;
4 an output path exists; 5 a writer or I/O failure. An error is the first line on stderr, as JSON.
";

struct Options {
    manifest: PathBuf,
    output: Output,
    manifest_options: ManifestOptions,
}

fn usage(detail: impl Into<String>) -> ConvertError {
    ConvertError { code: "usage", exit_code: 2, item_key: None, detail: detail.into() }
}

fn parse_args(args: Vec<String>) -> Result<Options, ConvertError> {
    let mut manifest = None;
    let mut output = None;
    let mut output_dir = None;
    let mut file_size = None;
    let mut excluded = None;
    let ManifestOptions { mut batch_items, mut batch_bytes, .. } = ManifestOptions::default();
    let mut args = args.into_iter();
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
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("pstsmith {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("convert") => return convert_main(args[1..].to_vec()),
        _ => {}
    }
    let options = match parse_args(args) {
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

struct ConvertOptions {
    inputs: Vec<PathBuf>,
    output: Output,
    manifest_options: ManifestOptions,
    temp_dir: Option<PathBuf>,
    json: bool,
}

fn parse_convert(args: Vec<String>) -> Result<ConvertOptions, ConvertError> {
    let mut inputs = Vec::new();
    let mut output = None;
    let mut output_dir = None;
    let mut file_size = None;
    let mut excluded = None;
    let mut temp_dir = None;
    let mut json = false;
    let ManifestOptions { mut batch_items, mut batch_bytes, .. } = ManifestOptions::default();
    let mut args = args.into_iter();
    let mut positional_only = false;
    while let Some(arg) = args.next() {
        if positional_only || !arg.starts_with('-') || arg == "-" {
            inputs.push(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--json" => json = true,
            flag => {
                let value = args.next().ok_or_else(|| usage(format!("missing value for {flag}")))?;
                match flag {
                    "-o" | "--output" => output = Some(PathBuf::from(value)),
                    "--output-dir" => output_dir = Some(PathBuf::from(value)),
                    "--max-file-size" => {
                        let size = FILE_SIZE_CHOICES.iter().find(|(name, _)| *name == value).map(|(_, size)| *size);
                        file_size = Some(size.ok_or_else(|| usage("--max-file-size must be 1GB, 2GB, 5GB or 10GB"))?);
                    }
                    #[cfg(feature = "test-file-sizes")]
                    "--max-file-bytes" => file_size = Some(value.parse().ok().filter(|size| *size > 0).ok_or_else(|| usage("--max-file-bytes must be a positive integer"))?),
                    "--excluded" => excluded = Some(PathBuf::from(value)),
                    "--temp-dir" => temp_dir = Some(PathBuf::from(value)),
                    "--batch" => batch_items = value.parse().map_err(|_| usage("--batch must be a positive integer"))?,
                    "--batch-bytes" => batch_bytes = value.parse().map_err(|_| usage("--batch-bytes must be a positive integer"))?,
                    other => return Err(usage(format!("unknown argument {other}"))),
                }
            }
        }
    }
    if batch_items == 0 || batch_bytes == 0 {
        return Err(usage("batch limits must be positive"));
    }
    let output = match (output, output_dir) {
        (Some(path), None) => Output::File(path),
        (None, Some(path)) => Output::Directory(path),
        _ => return Err(usage("usage: pstsmith convert INPUT... (-o FILE.pst | --output-dir DIR) [--max-file-size SIZE] (see --help)")),
    };
    if inputs.is_empty() {
        return Err(usage("usage: pstsmith convert INPUT... needs at least one input (see --help)"));
    }
    Ok(ConvertOptions { inputs, output, manifest_options: ManifestOptions { max_file_size: file_size, excluded, batch_items, batch_bytes }, temp_dir, json })
}

fn convert_main(args: Vec<String>) -> ExitCode {
    let options = match parse_convert(args) {
        Ok(options) => options,
        Err(error) => return fail(error),
    };
    let run = || -> Result<Report, ConvertError> {
        let mut converter = match &options.temp_dir {
            Some(dir) => Converter::create_in(options.output.clone(), &options.manifest_options, dir)?,
            None => Converter::create(options.output.clone(), &options.manifest_options)?,
        };
        for input in &options.inputs {
            converter.add_path(input)?;
        }
        converter.finish()
    };
    match run() {
        Ok(report) => {
            let outputs = report.outputs();
            let place = match (&options.output, outputs.len()) {
                (Output::File(path), _) => path.display().to_string(),
                (Output::Directory(path), parts) => format!("{parts} files in {}", path.display()),
            };
            let skipped = if report.skipped() > 0 { format!(" ({} left out, see the exclusion record)", report.skipped()) } else { String::new() };
            let messages = if report.written() == 1 { "1 message".to_string() } else { format!("{} messages", report.written()) };
            eprintln!("pstsmith: wrote {messages}{skipped} to {place}");
            for warning in report.warnings() {
                eprintln!("warning: {warning}");
            }
            if options.json {
                println!("{}", report.result_line());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            // After the error line: callers read stderr's first line as the error.
            let hint = convert_hint(&error, &options.output);
            let code = fail(error);
            if let Some(hint) = hint {
                eprintln!("{hint}");
            }
            code
        }
    }
}

/// What to do next, for a convert error a person can act on.
fn convert_hint(error: &ConvertError, output: &Output) -> Option<&'static str> {
    (error.code == "output-too-large" && matches!(output, Output::File(_))).then_some(
        "pstsmith: -o writes one file of at most --max-file-size (10GB by default); --output-dir DIR writes as many parts as the mail needs",
    )
}

fn fail(error: ConvertError) -> ExitCode {
    eprintln!("{}", error.error_line());
    ExitCode::from(error.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_file_that_is_too_large_points_to_parts() {
        let too_large = ConvertError { code: "output-too-large", exit_code: 2, item_key: None, detail: String::new() };
        let hint = convert_hint(&too_large, &Output::File("archive.pst".into())).expect("a hint for one file");
        assert!(hint.contains("--output-dir"), "{hint}");
        assert_eq!(convert_hint(&too_large, &Output::Directory("parts".into())), None);
        let other = ConvertError { code: "input-unreadable", ..too_large };
        assert_eq!(convert_hint(&other, &Output::File("archive.pst".into())), None);
    }
}
