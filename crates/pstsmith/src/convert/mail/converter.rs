//! The mail converter: EML files, MBOX files, Maildir directories, Thunderbird and Apple Mail folders to PST files.
//!
//! Each message is mapped (`mime.rs`) and written as one item of a manifest (`pst-export-manifest/2`) in a private
//! staging directory, its attachments as files beside it; [`Converter::finish`] then converts that manifest with
//! [`convert_manifest`](crate::convert::convert_manifest), unchanged, and removes the staging directory. Memory stays
//! bounded by the largest single message; the staging directory needs about as much free space as the decoded input.

use super::mime::{message_from_mime, MailItem};
use super::{sha256_hex, Origin, Stage, Staged};
use crate::convert::{convert_manifest_until, ConvertError, ManifestOptions, Output, Report};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// The Maildir info suffix (`:2,` then the flags, in Maildir++ also `!2,` on systems that forbid colons).
const MAILDIR_INFO: [&str; 2] = [":2,", "!2,"];

/// Converts mail into PST files. Add inputs, then [`finish`](Converter::finish).
///
/// ```no_run
/// use pstsmith::convert::{mail::Converter, ManifestOptions, Output};
/// # fn main() -> Result<(), pstsmith::convert::ConvertError> {
/// let mut converter = Converter::create(Output::File("archive.pst".into()), &ManifestOptions::default())?;
/// converter.add_path("Takeout/Mail/All mail Including Spam and Trash.mbox".as_ref())?;
/// let report = converter.finish()?;
/// println!("{} messages", report.written());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Converter {
    output: Output,
    options: ManifestOptions,
    staging: Staging,
    items: BufWriter<File>,
    folders: BTreeSet<Vec<String>>,
    /// The next ordinal of each container, so item keys and search keys stay distinct when two inputs share a name.
    ordinals: BTreeMap<String, u64>,
    count: u64,
    warnings: Vec<String>,
    expunged: u64,
    skipped_files: u64,
    linked_directories: u64,
    cancel: Option<Arc<AtomicBool>>,
}

impl Converter {
    /// A converter writing to `output` (a path that must not exist yet), staging in the system's temporary directory.
    ///
    /// # Errors
    ///
    /// `output-exists` when `output` or the exclusion record path exists; `io` when the staging directory cannot be
    /// created.
    pub fn create(output: Output, options: &ManifestOptions) -> Result<Self, ConvertError> {
        Self::create_in(output, options, &std::env::temp_dir())
    }

    /// As [`create`](Converter::create), staging in `temp_dir`.
    ///
    /// # Errors
    ///
    /// As [`create`](Converter::create).
    pub fn create_in(output: Output, options: &ManifestOptions, temp_dir: &Path) -> Result<Self, ConvertError> {
        let path = match &output {
            Output::File(path) | Output::Directory(path) => path,
        };
        if path.exists() {
            return Err(error("output-exists", 4, "output path already exists; the caller owns a fresh temporary path"));
        }
        if options.excluded.as_ref().is_some_and(|path| path.exists()) {
            return Err(error("output-exists", 4, "exclusion record path already exists; the caller owns a fresh temporary path"));
        }
        let staging = Staging::create(temp_dir).map_err(io_error)?;
        let items = BufWriter::new(staging.private_file("items.jsonl").map_err(io_error)?);
        Ok(Self {
            output,
            options: options.clone(),
            staging,
            items,
            folders: BTreeSet::new(),
            ordinals: BTreeMap::new(),
            count: 0,
            warnings: Vec::new(),
            expunged: 0,
            skipped_files: 0,
            linked_directories: 0,
            cancel: None,
        })
    }

    /// The private directory this converter stages decoded messages in. [`finish`](Converter::finish) removes it, and
    /// so does dropping the converter.
    pub fn staging_dir(&self) -> &Path {
        &self.staging.dir
    }

    /// Stops the conversion once `flag` is true, for example from a signal handler: the next message added, or the
    /// next item [`finish`](Converter::finish) writes, fails with `interrupted`. As on any error, `finish` removes the
    /// files it created, and dropping the converter removes the staging directory; nothing else is touched.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }

    fn cancelled(&self) -> Result<(), ConvertError> {
        match &self.cancel {
            Some(flag) if flag.load(Ordering::SeqCst) => Err(error("interrupted", 130, "the conversion was stopped")),
            _ => Ok(()),
        }
    }

    /// Adds an input, recognised by what it is:
    /// - an MBOX file (it starts with "From "): its messages go to a folder named after the file, without `.mbox`;
    /// - another file: one message (an `.eml`), in a folder named after its directory;
    /// - a Maildir or Maildir++ directory (it has `cur` and `new`): its messages go to "Inbox", its `.A.B` folders to
    ///   `A/B`;
    /// - any other directory, walked in name order: `.eml` files in a folder per directory (those at its top in a
    ///   folder named after it), MBOX files, Thunderbird `Name.sbd` and Apple Mail `Name.mbox` folders as subfolders,
    ///   Maildirs as folders; other files are skipped and counted.
    ///
    /// # Errors
    ///
    /// `input-unreadable` (exit 2) when the input cannot be read; `io` when staging fails.
    pub fn add_path(&mut self, input: &Path) -> Result<(), ConvertError> {
        let metadata = fs::metadata(input).map_err(|e| error("input-unreadable", 2, format!("{}: {e}", input.display())))?;
        // An input such as ".", ".." or "/" has no name of its own: the directory it stands for gives it.
        let name = Some(file_name(input))
            .filter(|name| !name.is_empty())
            .or_else(|| input.canonicalize().ok().map(|path| file_name(&path)).filter(|name| !name.is_empty()))
            .unwrap_or_else(|| "Imported".to_string());
        if metadata.is_dir() {
            if is_maildir(input) {
                self.add_maildir(input, &name, &["Inbox".to_string()], &[])
            } else {
                self.walk(input, &name, &[], &name)
            }
        } else if is_mbox(input).map_err(|e| unreadable(input, e))? {
            let folder = vec![mbox_folder_name(&name)];
            self.add_mbox(input, &name, &folder)
        } else {
            let parent = input.canonicalize().ok().and_then(|path| path.parent().map(file_name)).filter(|name| !name.is_empty());
            let folder = vec![parent.unwrap_or_else(|| "Imported".to_string())];
            let raw = fs::read(input).map_err(|e| unreadable(input, e))?;
            self.add_message(&raw, &name, &folder, None, None, false)
        }
    }

    /// Adds one message given as bytes (an `.eml`), to the folder path `folder` relative to the top of the store.
    ///
    /// # Errors
    ///
    /// `invalid-input` (exit 2) for an empty folder path or name; `io` when staging fails.
    pub fn add_eml(&mut self, folder: &[&str], raw: &[u8]) -> Result<(), ConvertError> {
        if folder.is_empty() || folder.iter().any(|name| name.is_empty()) {
            return Err(error("invalid-input", 2, "folder must be a non-empty list of non-empty names"));
        }
        let folder: Vec<String> = folder.iter().map(|name| name.to_string()).collect();
        let container = folder.join("/");
        self.add_message(raw, &container, &folder, None, None, false)
    }

    /// Converts everything added and returns the report. Everything this conversion created is removed on error.
    ///
    /// # Errors
    ///
    /// `empty-selection` when no message and no folder was found; the errors of
    /// [`convert_manifest`](crate::convert::convert_manifest).
    pub fn finish(mut self) -> Result<Report, ConvertError> {
        self.items.flush().map_err(io_error)?;
        if self.folders.is_empty() {
            return Err(error("empty-selection", 2, "no message or mail folder found in the inputs"));
        }
        if self.expunged > 0 {
            let (count, verb) = if self.expunged == 1 { ("1 message".to_string(), "was") } else { (format!("{} messages", self.expunged), "were") };
            self.warnings.push(format!("{count} deleted in Thunderbird but not yet compacted {verb} not converted"));
        }
        if self.linked_directories > 0 {
            let (count, verb) =
                if self.linked_directories == 1 { ("1 symbolic link".to_string(), "was") } else { (format!("{} symbolic links", self.linked_directories), "were") };
            self.warnings.push(format!("{count} to a directory {verb} not followed"));
        }
        if self.skipped_files > 0 {
            let (count, verb) = if self.skipped_files == 1 { ("1 file that is".to_string(), "was") } else { (format!("{} files that are", self.skipped_files), "were") };
            self.warnings.push(format!("{count} not mail {verb} skipped"));
        }
        let mut by_case: BTreeMap<Vec<String>, Vec<&Vec<String>>> = BTreeMap::new();
        for folder in &self.folders {
            by_case.entry(folder.iter().map(|name| name.to_lowercase()).collect()).or_default().push(folder);
        }
        for variants in by_case.values().filter(|variants| variants.len() > 1) {
            let names: Vec<String> = variants.iter().map(|path| path.join("/")).collect();
            self.warnings.push(format!(
                "folders {} differ only in case; Exchange (and so a Microsoft 365 PST import) merges them",
                names.join(", ")
            ));
        }
        let manifest = self.staging.dir.join("manifest.jsonl");
        let write = || -> io::Result<()> {
            let mut out = BufWriter::new(self.staging.private_file("manifest.jsonl")?);
            let header = json!({ "schema": "pst-export-manifest/2", "export_id": "pstsmith-mail", "item_count": self.count, "folder_count": self.folders.len() });
            writeln!(out, "{header}")?;
            for folder in &self.folders {
                writeln!(out, "{}", json!({ "folder_path": folder }))?;
            }
            io::copy(&mut File::open(self.staging.dir.join("items.jsonl"))?, &mut out)?;
            out.flush()
        };
        write().map_err(io_error)?;
        let report = convert_manifest_until(&manifest, &self.output, &self.options, self.cancel.clone())?;
        Ok(Report { warnings: std::mem::take(&mut self.warnings), ..report })
    }

    fn add_message(&mut self, raw: &[u8], container: &str, folder: &[String], mbox_date: Option<i64>, seen: Option<bool>, draft: bool) -> Result<(), ConvertError> {
        self.cancelled()?;
        let ordinal = self.ordinals.entry(container.to_string()).or_insert(0);
        *ordinal += 1;
        let origin = Origin { container: container.to_string(), ordinal: *ordinal, mbox_date, seen, draft };
        let item_key = format!("{container}#{}", origin.ordinal);
        let MailItem { message, folder: label_folder, warnings } = message_from_mime(raw, &origin, &mut self.staging).map_err(io_error)?;
        let folder = label_folder.map(|name| vec![name]).unwrap_or_else(|| folder.to_vec());
        for depth in 1..=folder.len() {
            self.folders.insert(folder[..depth].to_vec());
        }
        let mut item = message;
        let object = item.as_object_mut().expect("a mapped message is an object");
        object.insert("item_key".to_string(), json!(item_key));
        object.insert("folder_path".to_string(), json!(folder));
        serde_json::to_writer(&mut self.items, &item).map_err(|e| io_error(e.into()))?;
        self.items.write_all(b"\n").map_err(io_error)?;
        self.count += 1;
        self.warnings.extend(warnings.into_iter().map(|warning| format!("{item_key}: {warning}")));
        Ok(())
    }

    fn add_mbox(&mut self, path: &Path, container: &str, folder: &[String]) -> Result<(), ConvertError> {
        self.folders.insert(folder.to_vec());
        for depth in 1..folder.len() {
            self.folders.insert(folder[..depth].to_vec());
        }
        let file = File::open(path).map_err(|e| unreadable(path, e))?;
        let mut entries = MboxEntries { reader: BufReader::new(file), line: Vec::new(), ahead: Vec::new(), pending: None, first: true };
        while let Some(entry) = entries.next_entry().map_err(|e| unreadable(path, e))? {
            let (raw, date) = entry;
            let raw = without_separator_line(&raw);
            if thunderbird_expunged(raw) {
                self.expunged += 1;
                continue;
            }
            self.add_message(raw, container, folder, date, None, false)?;
        }
        Ok(())
    }

    /// A Maildir: `cur` and `new` in name order (new messages are unseen), then its Maildir++ folders `.A.B`, their
    /// names decoded from modified UTF-7. A symbolic link among the folders is not followed and is counted.
    fn add_maildir(&mut self, dir: &Path, container: &str, folder: &[String], prefix: &[String]) -> Result<(), ConvertError> {
        self.folders.insert(folder.to_vec());
        for depth in 1..folder.len() {
            self.folders.insert(folder[..depth].to_vec());
        }
        for sub in ["cur", "new"] {
            for entry in sorted_entries(&dir.join(sub)).map_err(|e| unreadable(dir, e))? {
                let name = file_name(&entry);
                // Maildir message names never start with a dot; such a file is a placeholder or a lock.
                if !entry.is_file() || name.starts_with('.') {
                    continue;
                }
                let flags = MAILDIR_INFO.iter().find_map(|info| name.split_once(info).map(|(_, flags)| flags.to_string()));
                let seen = sub == "cur" && flags.as_deref().is_some_and(|flags| flags.contains('S'));
                let draft = flags.as_deref().is_some_and(|flags| flags.contains('D'));
                let raw = fs::read(&entry).map_err(|e| unreadable(&entry, e))?;
                self.add_message(&raw, &format!("{container}/{sub}"), folder, None, Some(seen), draft)?;
            }
        }
        for entry in sorted_entries(dir).map_err(|e| unreadable(dir, e))? {
            let name = file_name(&entry);
            if let Some(sub) = name.strip_prefix('.').filter(|sub| !sub.is_empty() && is_maildir(&entry)) {
                if fs::symlink_metadata(&entry).map_err(|e| unreadable(&entry, e))?.file_type().is_symlink() {
                    self.linked_directories += 1;
                    continue;
                }
                let mut path = prefix.to_vec();
                path.extend(sub.split('.').filter(|part| !part.is_empty()).map(|part| modified_utf7(part).unwrap_or_else(|| part.to_string())));
                if path.len() > prefix.len() {
                    self.add_maildir(&entry, &format!("{container}/{name}"), &path, &path)?;
                }
            }
        }
        Ok(())
    }

    /// A directory that is not a Maildir: its entries in name order. A symbolic link to a file is read; one to a
    /// directory is not followed (it could lead back up the tree) and is counted.
    fn walk(&mut self, dir: &Path, container: &str, folder: &[String], top: &str) -> Result<(), ConvertError> {
        for entry in sorted_entries(dir).map_err(|e| unreadable(dir, e))? {
            let name = file_name(&entry);
            if name.starts_with('.') || companion_file(&name) {
                continue;
            }
            let link = fs::symlink_metadata(&entry).map_err(|e| unreadable(&entry, e))?.file_type().is_symlink();
            let entry_container = format!("{container}/{name}");
            let mut sub_folder = folder.to_vec();
            if entry.is_dir() {
                if link {
                    self.linked_directories += 1;
                    continue;
                }
                let stem = name.strip_suffix(".sbd").or_else(|| name.strip_suffix(".mbox")).unwrap_or(&name).to_string();
                sub_folder.push(stem);
                if is_maildir(&entry) {
                    self.add_maildir(&entry, &entry_container, &sub_folder, &sub_folder)?;
                } else {
                    self.walk(&entry, &entry_container, &sub_folder, top)?;
                }
            } else if entry.is_file() {
                let here = if folder.is_empty() { vec![top.to_string()] } else { folder.to_vec() };
                let empty_thunderbird_folder =
                    fs::metadata(&entry).is_ok_and(|metadata| metadata.len() == 0) && dir.join(format!("{name}.msf")).is_file();
                if name.to_ascii_lowercase().ends_with(".eml") {
                    let raw = fs::read(&entry).map_err(|e| unreadable(&entry, e))?;
                    self.add_message(&raw, &entry_container, &here, None, None, false)?;
                } else if empty_thunderbird_folder || is_mbox(&entry).map_err(|e| unreadable(&entry, e))? {
                    // Apple Mail keeps a mailbox as `Name.mbox/mbox`: the folder is the directory's.
                    let target = if name == "mbox" { here } else { [folder, &[mbox_folder_name(&name)][..]].concat() };
                    self.add_mbox(&entry, &entry_container, &target)?;
                } else {
                    self.skipped_files += 1;
                }
            }
        }
        Ok(())
    }
}

/// A Maildir++ folder name as Dovecot and Courier store it by default: IMAP's modified UTF-7 (RFC 3501 5.1.3), where
/// "&" starts base64 of UTF-16 (with "," for "/") that runs to "-", and "&-" is "&". `None` when the name is not valid
/// modified UTF-7, as in a store kept in UTF-8; the caller then uses the name as it is.
fn modified_utf7(name: &str) -> Option<String> {
    if !name.is_ascii() {
        return None;
    }
    let mut out = String::new();
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let (encoded, after) = rest[at + 1..].split_once('-')?;
        rest = after;
        if encoded.is_empty() {
            out.push('&');
            continue;
        }
        let (mut bits, mut count, mut units) = (0u32, 0u32, Vec::new());
        for byte in encoded.bytes() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b',' => 63,
                _ => return None,
            };
            bits = bits << 6 | u32::from(value);
            count += 6;
            if count >= 16 {
                count -= 16;
                units.push((bits >> count) as u16);
                bits &= (1 << count) - 1;
            }
        }
        // What is left must be fewer than 6 bits, all zero; printable ASCII is never encoded.
        let text = String::from_utf16(&units).ok().filter(|_| count < 6 && bits == 0)?;
        if text.is_empty() || text.chars().any(|c| (' '..='~').contains(&c)) {
            return None;
        }
        out.push_str(&text);
    }
    out.push_str(rest);
    Some(out)
}

/// Files that come with mail folders and are not mail: Thunderbird's `.msf` indexes, Apple Mail's `Info.plist` and
/// `table_of_contents`.
fn companion_file(name: &str) -> bool {
    name.ends_with(".msf") || name == "Info.plist" || name == "table_of_contents"
}

/// The entries of an MBOX file, read as a stream: each one's bytes (mboxrd: one '>' removed from a quoted "From "
/// line) and the date of its "From " line. A line starting with "From " starts an entry only when it is the file's
/// first, when it looks like an MBOX "From " line (a sender, a time and a four-digit year), or when its date is written
/// as in a Date header and a blank line comes before it and a header field after it: an unquoted "From " in a body
/// (mboxo) does not cut its message.
struct MboxEntries<R> {
    reader: R,
    line: Vec<u8>,
    /// The line after a "From " line, read ahead to see whether it is a header field; the next line to take.
    ahead: Vec<u8>,
    pending: Option<(Vec<u8>, Option<i64>)>,
    first: bool,
}

impl<R: BufRead> MboxEntries<R> {
    fn next_entry(&mut self) -> io::Result<Option<(Vec<u8>, Option<i64>)>> {
        loop {
            self.line.clear();
            if self.ahead.is_empty() {
                if self.reader.read_until(b'\n', &mut self.line)? == 0 {
                    return Ok(self.pending.take());
                }
            } else {
                std::mem::swap(&mut self.line, &mut self.ahead);
            }
            if self.first {
                if self.line.starts_with(b"\xEF\xBB\xBF") {
                    self.line.drain(..3);
                }
                if self.line == b"\n" || self.line == b"\r\n" {
                    continue;
                }
            }
            let date_style = self.line.starts_with(b"From ").then(|| date_header_style(&self.line)).flatten();
            if self.line.starts_with(b"From ")
                && (self.first || from_line_date(&self.line).is_some() || (date_style.is_some() && self.header_follows_a_blank_line()?))
            {
                self.first = false;
                let date = from_line_date(&self.line).flatten().or(date_style);
                if let Some(done) = self.pending.replace((Vec::new(), date)) {
                    return Ok(Some(done));
                }
                continue;
            }
            self.first = false;
            if let Some((contents, _)) = self.pending.as_mut() {
                let line = &self.line;
                let quoted = line.iter().position(|byte| *byte != b'>').is_some_and(|at| at > 0 && line[at..].starts_with(b"From "));
                contents.extend_from_slice(if quoted { &line[1..] } else { line });
            }
        }
    }

    /// For a "From " line whose date is not in the usual form: whether a blank line ends the entry before it and the
    /// line after it is a header field. That line is read ahead (here `ahead` is always empty: the line in hand is the
    /// one read ahead before, if any) and is taken next.
    fn header_follows_a_blank_line(&mut self) -> io::Result<bool> {
        let blank_before = self.pending.as_ref().is_some_and(|(contents, _)| contents.ends_with(b"\n\n") || contents.ends_with(b"\n\r\n"));
        if !blank_before {
            return Ok(false);
        }
        self.reader.read_until(b'\n', &mut self.ahead)?;
        // RFC 5322 2.2: a field name is printable US-ASCII without the colon, then the colon.
        let name = self.ahead.iter().position(|byte| *byte == b':').map(|at| &self.ahead[..at]);
        Ok(name.is_some_and(|name| !name.is_empty() && name.iter().all(|byte| (33..=126).contains(byte))))
    }
}

/// A "From " line whose date is written as in a Date header ("From sender Sat, 01 Jan 2024 11:00:00 +0000"): its date,
/// when the words after the sender have a four-digit year and a time and mail-parser reads them as an RFC 5322 date.
fn date_header_style(line: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(line).ok()?.trim_end();
    let (_, date) = text.strip_prefix("From ")?.trim_start().split_once(char::is_whitespace)?;
    let words: Vec<&str> = date.split_whitespace().collect();
    let year = words.iter().any(|word| word.len() == 4 && word.bytes().all(|byte| byte.is_ascii_digit()));
    let time = words.iter().any(|word| word.contains(':') && word.bytes().all(|byte| byte.is_ascii_digit() || byte == b':'));
    if !year || !time {
        return None;
    }
    mail_parser::DateTime::parse_rfc822(date.trim()).filter(mail_parser::DateTime::is_valid).map(|date| date.to_timestamp())
}

/// An MBOX "From " line's date: `Some(date)` when the line has a sender, a time and a four-digit year at its end
/// ("From sender Mon Jan  1 10:00:00 2024", also with a time zone before the year), `Some(None)` when its date cannot
/// be read, `None` when the line is no MBOX "From " line.
fn from_line_date(line: &[u8]) -> Option<Option<i64>> {
    let text = std::str::from_utf8(line).ok()?.trim_end();
    let mut words = text.strip_prefix("From ")?.split_whitespace();
    words.next()?;
    let rest: Vec<&str> = words.collect();
    let year: u16 = rest.last().filter(|year| year.len() == 4).and_then(|year| year.parse().ok())?;
    let time = rest.iter().find(|word| word.matches(':').count() >= 1 && word.chars().all(|c| c.is_ascii_digit() || c == ':'))?;
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let month = rest.iter().find_map(|word| MONTHS.iter().position(|m| word.eq_ignore_ascii_case(m)));
    let day = rest.iter().find_map(|word| word.parse::<u8>().ok().filter(|day| (1..=31).contains(day)));
    let mut clock = time.split(':').map(|part| part.parse::<u8>().ok());
    let (hour, minute, second) = (clock.next().flatten(), clock.next().flatten(), clock.next().flatten().unwrap_or(0));
    let date = match (month, day, hour, minute) {
        (Some(month), Some(day), Some(hour), Some(minute)) => {
            let date = mail_parser::DateTime {
                year,
                month: month as u8 + 1,
                day,
                hour,
                minute,
                second,
                tz_before_gmt: false,
                tz_hour: 0,
                tz_minute: 0,
            };
            date.is_valid().then(|| date.to_timestamp())
        }
        _ => None,
    };
    Some(date)
}

fn error(code: &'static str, exit_code: u8, detail: impl Into<String>) -> ConvertError {
    ConvertError { code, exit_code, item_key: None, detail: detail.into() }
}

fn io_error(e: io::Error) -> ConvertError {
    error("io", 5, format!("{}: {e}", e.kind()))
}

fn unreadable(path: &Path, e: io::Error) -> ConvertError {
    error("input-unreadable", 2, format!("{}: {e}", path.display()))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn mbox_folder_name(name: &str) -> String {
    let stem = [".mbox", ".mbx", ".MBOX", ".MBX"].iter().find_map(|ext| name.strip_suffix(ext)).unwrap_or(name);
    if stem.is_empty() { name.to_string() } else { stem.to_string() }
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn is_maildir(dir: &Path) -> bool {
    dir.join("cur").is_dir() && dir.join("new").is_dir()
}

/// A file is an MBOX when its first line that is not empty (after a UTF-8 byte order mark) starts with "From ".
fn is_mbox(path: &Path) -> io::Result<bool> {
    let mut start = Vec::with_capacity(4096);
    File::open(path)?.take(4096).read_to_end(&mut start)?;
    let start = start.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&start);
    let first = start.iter().position(|byte| *byte != b'\r' && *byte != b'\n').map_or(&[][..], |at| &start[at..]);
    Ok(first.starts_with(b"From "))
}

/// An MBOX entry without the blank line that separates it from the next "From " line.
fn without_separator_line(contents: &[u8]) -> &[u8] {
    if contents.ends_with(b"\r\n\r\n") {
        &contents[..contents.len() - 2]
    } else if contents.ends_with(b"\n\n") {
        &contents[..contents.len() - 1]
    } else {
        contents
    }
}

/// Thunderbird marks a deleted message with X-Mozilla-Status bit 0x0008 and removes it only when the folder is
/// compacted; such a message is not converted.
fn thunderbird_expunged(raw: &[u8]) -> bool {
    for line in raw.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return false;
        }
        if let Some(value) = line.get(..17).filter(|name| name.eq_ignore_ascii_case(b"X-Mozilla-Status:")).map(|_| &line[17..]) {
            return std::str::from_utf8(value).ok().and_then(|value| u32::from_str_radix(value.trim(), 16).ok()).is_some_and(|status| status & 0x0008 != 0);
        }
    }
    false
}

/// A private directory for the staged manifest and attachments, removed when dropped.
#[derive(Debug)]
struct Staging {
    dir: PathBuf,
    next: u64,
}

impl Staging {
    fn create(parent: &Path) -> io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = parent.join(format!("pstsmith-{}-{stamp}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
            // The staged messages are private mail: only the user may read them (0700 on Unix).
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Ok(Self { dir, next: 0 }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Staging {
    /// A new file in the staging directory, readable by the user only (0600 on Unix).
    fn private_file(&self, name: &str) -> io::Result<File> {
        let mut options = File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(self.dir.join(name))
    }
}

impl Stage for Staging {
    fn stage(&mut self, data: &[u8]) -> io::Result<Staged> {
        self.next += 1;
        let name = format!("a{}", self.next);
        self.private_file(&name)?.write_all(data)?;
        let path = self.dir.join(name);
        let path = path.into_os_string().into_string().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the temporary directory's path is not UTF-8"))?;
        Ok(Staged { path, size: data.len() as u64, sha256: sha256_hex(data) })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_header_style_reads_only_dates() {
        assert_eq!(date_header_style(b"From a@example.invalid Sat, 01 Jan 2024 11:00:00 +0000\n"), Some(1_704_106_800));
        assert_eq!(date_header_style(b"From a@example.invalid Mon, 1 Jan 2024 10:00:00 -0500\r\n"), Some(1_704_121_200));
        for body in ["From 9:00 to 17:00\r\n", "From our website\r\n", "From me 12:30 today\n", "From here on 2024\n", "From a Mon Jan 1\n"] {
            assert_eq!(date_header_style(body.as_bytes()), None, "{body:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn staging_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!("pstsmith-staging-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        let mut staging = Staging::create(&parent).unwrap();
        assert_eq!(fs::metadata(&staging.dir).unwrap().permissions().mode() & 0o777, 0o700);
        let staged = staging.stage(b"private mail").unwrap();
        assert_eq!(fs::metadata(&staged.path).unwrap().permissions().mode() & 0o077, 0);
        drop(staging);
        fs::remove_dir_all(&parent).unwrap();
    }
}
