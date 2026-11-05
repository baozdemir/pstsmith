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
    // Declared before `staging`: fields drop in order, and on Windows the staging directory can be removed only once
    // `items.jsonl` in it is closed.
    items: BufWriter<File>,
    staging: Staging,
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
    /// `output-exists` when `output` or the exclusion record path exists; `io` when the directory of either is missing
    /// or not a directory, or the staging directory cannot be created.
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
        // Found now, not after the whole input is staged.
        for (target, what) in std::iter::once((path, "output")).chain(options.excluded.as_ref().map(|path| (path, "exclusion record"))) {
            if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                let problem = match fs::metadata(parent) {
                    Ok(metadata) if metadata.is_dir() => None,
                    Ok(_) => Some("is not a directory".to_string()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Some("does not exist".to_string()),
                    Err(e) => Some(format!("cannot be reached: {e}")),
                };
                if let Some(problem) = problem {
                    return Err(error("io", 5, format!("the {what}'s directory {} {problem}", parent.display())));
                }
            }
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

    /// Stops the conversion once `flag` is true, for example from a signal handler: the next message added, the next
    /// item [`finish`](Converter::finish) writes, or the attachment being written fails with `interrupted`, and the
    /// conversion does not finish. As on any error, `finish` removes the files it created, and dropping the
    /// converter removes the staging directory; nothing else is touched. A flag set after `finish` has closed the last
    /// file stops nothing: the files are then whole.
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
    /// - an MBOX file (it starts with "From "): its messages go to a folder named after the file, without `.mbox`
    ///   (Apple Mail's `Name.mbox/mbox`: `Name`);
    /// - another file: one message (an `.eml`), in a folder named after its directory;
    /// - a Maildir or Maildir++ directory (it has `cur` and `new`): its messages go to "Inbox", its `.A.B` folders to
    ///   `A/B`;
    /// - any other directory, walked in name order: `.eml` files in a folder per directory (those at its top in a
    ///   folder named after it), MBOX files, Thunderbird `Name.sbd` and Apple Mail `Name.mbox` folders as subfolders,
    ///   Maildirs as folders; other files are skipped and counted. A `Name.sbd` or `Name.mbox` directory given on its
    ///   own is named `Name`.
    ///
    /// # Errors
    ///
    /// `input-unreadable` (exit 2) when the input cannot be read; `input-unsupported` (exit 2) for a PST file;
    /// `interrupted` once the cancel flag is set; `io` when staging fails.
    pub fn add_path(&mut self, input: &Path) -> Result<(), ConvertError> {
        let metadata = fs::metadata(input).map_err(|e| error("input-unreadable", 2, format!("{}: {e}", input.display())))?;
        // An input such as ".", ".." or "/" has no name of its own: the directory it stands for gives it.
        let name = real_name(input).unwrap_or_else(|| "Imported".to_string());
        if metadata.is_dir() {
            // A Thunderbird `Name.sbd` or Apple Mail `Name.mbox` directory given on its own is named `Name` (its
            // mailboxes become folders at the top, as in any directory; Apple Mail's `mbox` file is the folder `Name`).
            let name = name.strip_suffix(".sbd").or_else(|| name.strip_suffix(".mbox")).filter(|stem| !stem.is_empty()).unwrap_or(&name).to_string();
            if is_maildir(input) {
                self.add_maildir(input, &name, &["Inbox".to_string()], &[])
            } else {
                self.walk(input, &name, &[], &name)
            }
        } else if is_pst(input).map_err(|e| unreadable(input, e))? {
            Err(error("input-unsupported", 2, format!("{}: a PST file is not mail input; pstsmith writes PST files from mail", input.display())))
        } else if is_mbox(input).map_err(|e| unreadable(input, e))? {
            // Apple Mail's `Name.mbox/mbox` is the folder `Name`, as when its directory is walked.
            let folder = vec![apple_mailbox(input).unwrap_or_else(|| mbox_folder_name(&name))];
            let routed = labels_route(input);
            self.add_mbox(input, &name, &folder, routed)
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
    /// `invalid-input` (exit 2) for an empty folder path or name; `interrupted` once the cancel flag is set; `io` when
    /// staging fails.
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
    /// `empty-selection` when no message and no folder was found; `interrupted` once the cancel flag is set; the errors
    /// of [`convert_manifest`](crate::convert::convert_manifest).
    pub fn finish(mut self) -> Result<Report, ConvertError> {
        self.cancelled()?;
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
        self.add_routed(raw, container, folder, mbox_date, seen, draft, true)
    }

    /// As `add_message`; `routed` false keeps the message in `folder` whatever its Gmail labels (its labels that are not
    /// system folders still become categories, and Drafts and Unread still set its state).
    #[allow(clippy::too_many_arguments)]
    fn add_routed(&mut self, raw: &[u8], container: &str, folder: &[String], mbox_date: Option<i64>, seen: Option<bool>, draft: bool, routed: bool) -> Result<(), ConvertError> {
        self.cancelled()?;
        let ordinal = self.ordinals.entry(container.to_string()).or_insert(0);
        *ordinal += 1;
        let origin = Origin { container: container.to_string(), ordinal: *ordinal, mbox_date, seen, draft };
        let item_key = format!("{container}#{}", origin.ordinal);
        let MailItem { message, folder: label_folder, warnings } = message_from_mime(raw, &origin, &mut self.staging).map_err(io_error)?;
        let folder = label_folder.filter(|_| routed).map(|name| vec![name]).unwrap_or_else(|| folder.to_vec());
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

    /// The messages of an MBOX file, in `folder`; with `routed`, in the folders their Gmail labels name, and then an
    /// MBOX whose every message has a label adds no folder of its own (Google Takeout's "All mail Including Spam and
    /// Trash"). An MBOX without a message is a folder still (Thunderbird keeps an empty folder as an empty file, or one
    /// of deleted messages only).
    fn add_mbox(&mut self, path: &Path, container: &str, folder: &[String], routed: bool) -> Result<(), ConvertError> {
        let before = self.count;
        if !routed {
            for depth in 1..=folder.len() {
                self.folders.insert(folder[..depth].to_vec());
            }
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
            self.add_routed(raw, container, folder, date, None, false, routed)?;
        }
        if self.count == before {
            for depth in 1..=folder.len() {
                self.folders.insert(folder[..depth].to_vec());
            }
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
                let empty = fs::metadata(&entry).is_ok_and(|metadata| metadata.len() == 0);
                // An empty mailbox: Thunderbird's file beside its index, Apple Mail's `Name.mbox/mbox`.
                let empty_mail_folder =
                    empty && (dir.join(format!("{name}.msf")).is_file() || (name == "mbox" && real_name(dir).is_some_and(|dir| dir.ends_with(".mbox"))));
                if name.to_ascii_lowercase().ends_with(".eml") {
                    let raw = fs::read(&entry).map_err(|e| unreadable(&entry, e))?;
                    self.add_message(&raw, &entry_container, &here, None, None, false)?;
                } else if empty_mail_folder || is_mbox(&entry).map_err(|e| unreadable(&entry, e))? {
                    // Apple Mail keeps a mailbox as `Name.mbox/mbox`: the folder is the directory's.
                    let target = if name == "mbox" { here } else { [folder, &[mbox_folder_name(&name)][..]].concat() };
                    let routed = labels_route(&entry);
                    self.add_mbox(&entry, &entry_container, &target, routed)?;
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
/// first, when it looks like an MBOX "From " line (a sender, a month name, and a time just before a four-digit year
/// or before its zone words, see `from_line_date`), or
/// when its date is written
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

/// The words of a "From " line after its sender, split on spaces and tabs, without UUCP's trailing "remote from
/// host"; `None` when the line has no sender. A sender need not be UTF-8 (a Latin-1 address). The oracle
/// (`tests/oracle/compare_mail.py`) states this and the two rules below step by step in Python.
fn from_line_words(line: &[u8]) -> Option<Vec<String>> {
    let text = String::from_utf8_lossy(line);
    let rest = text.trim_end_matches(['\r', '\n']).strip_prefix("From ")?;
    let mut words: Vec<String> = rest.split([' ', '\t']).filter(|word| !word.is_empty()).map(str::to_string).collect();
    if words.is_empty() {
        return None;
    }
    words.remove(0);
    let n = words.len();
    if n >= 3 && words[n - 3] == "remote" && words[n - 2] == "from" {
        words.truncate(n - 3);
    }
    Some(words)
}

fn ascii_digits(word: &str, lengths: std::ops::RangeInclusive<usize>) -> bool {
    lengths.contains(&word.len()) && word.bytes().all(|byte| byte.is_ascii_digit())
}

/// `h:mm`, `hh:mm` or `hh:mm:ss`, as (hour, minute, second); ranges are checked with the date.
fn clock(word: &str) -> Option<(u8, u8, u8)> {
    let parts: Vec<&str> = word.split(':').collect();
    if !(2..=3).contains(&parts.len()) || !ascii_digits(parts[0], 1..=2) || parts[1..].iter().any(|part| !ascii_digits(part, 2..=2)) {
        return None;
    }
    let second = parts.get(2).map_or(Some(0), |part| part.parse().ok())?;
    Some((parts[0].parse().ok()?, parts[1].parse().ok()?, second))
}

fn is_month(word: &str) -> bool {
    MONTHS.iter().any(|month| word.eq_ignore_ascii_case(month))
}

/// A zone: `+hhmm` or `-hhmm` (hours to 23), or 1 to 5 capital letters that are not a month name (RFC 5322 4.3: UT,
/// GMT, Z and the North American names have their offsets; any other name is taken as UTC). As (west of GMT, hours,
/// minutes).
fn zone(word: &str) -> Option<(bool, u8, u8)> {
    if word.len() == 5 && (word.starts_with('+') || word.starts_with('-')) && ascii_digits(&word[1..], 4..=4) {
        let (hours, minutes) = (word[1..3].parse().ok()?, word[3..].parse().ok()?);
        return (hours <= 23 && minutes <= 59).then_some((word.starts_with('-'), hours, minutes));
    }
    if !(1..=5).contains(&word.len()) || !word.bytes().all(|byte| byte.is_ascii_uppercase()) || is_month(word) {
        return None;
    }
    Some(match word {
        "EST" => (true, 5, 0),
        "EDT" => (true, 4, 0),
        "CST" => (true, 6, 0),
        "CDT" => (true, 5, 0),
        "MST" => (true, 7, 0),
        "MDT" => (true, 6, 0),
        "PST" => (true, 8, 0),
        "PDT" => (true, 7, 0),
        _ => (false, 0, 0),
    })
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// A zone word between the time and the year, as System V's `%Z` writes it: a zone, tzdata's numeric abbreviation
/// `+hh`/`-hh` ("+03"), or 1 to 5 letters starting with a capital, with two capitals at least, that are not a month
/// name ("ChST", taken as UTC; a capitalised word such as "Room" is none).
fn zone_abbreviation(word: &str) -> Option<(bool, u8, u8)> {
    if let Some(found) = zone(word) {
        return Some(found);
    }
    if word.len() == 3 && (word.starts_with('+') || word.starts_with('-')) && ascii_digits(&word[1..], 2..=2) {
        let hours: u8 = word[1..].parse().ok()?;
        return (hours <= 23).then_some((word.starts_with('-'), hours, 0));
    }
    let letters = (1..=5).contains(&word.len())
        && word.bytes().all(|byte| byte.is_ascii_alphabetic())
        && word.as_bytes()[0].is_ascii_uppercase()
        && word.bytes().filter(u8::is_ascii_uppercase).count() >= 2;
    (letters && !is_month(word)).then_some((false, 0, 0))
}

/// The date, when it exists (a real calendar day, year 1900 to 3000, hours to 23, minutes and seconds to 59).
fn timestamp(year: u16, month: u8, day: u8, (hour, minute, second): (u8, u8, u8), (west, tz_hour, tz_minute): (bool, u8, u8)) -> Option<i64> {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) || day == 0 || day > days[usize::from(month) - 1] {
        return None;
    }
    let date = mail_parser::DateTime { year, month, day, hour, minute, second, tz_before_gmt: west, tz_hour, tz_minute };
    date.is_valid().then(|| date.to_timestamp())
}

/// A "From " line whose date is written as in a Date header (RFC 5322 3.3), "From sender Sat, 01 Jan 2024 11:00:00
/// +0000": its date, when the words after the sender are exactly `[day-of-week[,]] day month year hh:mm[:ss] [zone]
/// [(comment)]` and the date exists. Anything else, such as a body line that only starts like a date, is no such line.
fn date_header_style(line: &[u8]) -> Option<i64> {
    let mut words = from_line_words(line)?;
    if let Some(at) = words.iter().position(|word| word.starts_with('(')) {
        if !words.last().is_some_and(|word| word.ends_with(')')) {
            return None;
        }
        words.truncate(at);
    }
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let weekday = words.first().is_some_and(|word| DAYS.contains(&word.strip_suffix(',').unwrap_or(word).to_ascii_lowercase().as_str()));
    let rest = &words[usize::from(weekday)..];
    if !(4..=5).contains(&rest.len()) || !ascii_digits(&rest[0], 1..=2) || !ascii_digits(&rest[2], 4..=4) {
        return None;
    }
    let month = MONTHS.iter().position(|month| rest[1].eq_ignore_ascii_case(month))? as u8 + 1;
    let tz = match rest.get(4) {
        Some(word) => zone(word)?,
        None => (false, 0, 0),
    };
    timestamp(rest[2].parse().ok()?, month, rest[0].parse().ok()?, clock(&rest[3])?, tz)
}

/// An MBOX "From " line (asctime, as mail programs write it, with an English month name): `Some(date)` when the words
/// after the sender end with a four-digit year, a month name is among them, and the time comes just before the year
/// or before one or two zone words before it ("From sender Mon Jan  1 10:00:00 2024", Google Takeout's "... 10:01:00
/// +0000 2024", System V's "... 10:00 PST 2024" or "... 10:00 +03 2024", mutt's example "... 00:39:12 MET DST 1995"),
/// or just before the year with a zone after it (UW c-client, Alpine: "... 10:00:00 2024 +0000"); `Some(None)` when
/// its date cannot be read; `None` when the line is no MBOX "From " line. So a body line such as "From 10:00 to 12:00
/// on 15 March 2024" is none. Of the zone words, a numeric one gives the offset, else the one next to the year.
fn from_line_date(line: &[u8]) -> Option<Option<i64>> {
    let mut words = from_line_words(line)?;
    if !words.iter().any(|word| is_month(word)) {
        return None;
    }
    let mut tz = (false, 0, 0);
    let n = words.len();
    if n >= 3 && ascii_digits(&words[n - 2], 4..=4) && clock(&words[n - 3]).is_some() {
        if let Some(found) = zone(&words[n - 1]) {
            tz = found;
            words.pop();
        }
    }
    let year: u16 = words.last().filter(|word| ascii_digits(word, 4..=4))?.parse().ok()?;
    let before = &words[..words.len() - 1];
    let zones = before.iter().rev().take_while(|word| clock(word).is_none() && zone_abbreviation(word).is_some()).count();
    if zones > 2 {
        return None;
    }
    let time = clock(before.get(before.len().checked_sub(zones + 1)?)?)?;
    let zone_words = &before[before.len() - zones..];
    if let Some(word) = zone_words.iter().find(|word| word.starts_with('+') || word.starts_with('-')).or(zone_words.last()) {
        tz = zone_abbreviation(word).unwrap_or(tz);
    }
    let month = words.iter().find_map(|word| MONTHS.iter().position(|month| word.eq_ignore_ascii_case(month)))?;
    let day = words.iter().find(|word| ascii_digits(word, 1..=2)).and_then(|word| word.parse::<u8>().ok());
    Some(day.and_then(|day| timestamp(year, month as u8 + 1, day, time, tz)))
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

/// A path's own name, or for one such as ".", ".." or "./" the name of the directory it stands for.
fn real_name(path: &Path) -> Option<String> {
    Some(file_name(path))
        .filter(|name| !name.is_empty())
        .or_else(|| path.canonicalize().ok().map(|path| file_name(&path)).filter(|name| !name.is_empty()))
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

/// Whether an MBOX's Gmail labels choose its messages' folders: yes for an MBOX on its own (Google Takeout), no for a
/// mail program's folder, where the user filed the messages (Thunderbird's file beside its `.msf` index or inside a
/// `.sbd` directory, Apple Mail's `Name.mbox/mbox`).
fn labels_route(path: &Path) -> bool {
    let name = file_name(path);
    !(path.with_file_name(format!("{name}.msf")).is_file() || parent_name(path).ends_with(".sbd") || apple_mailbox(path).is_some())
}

/// The name of a path's directory; the parent of a bare "mbox" is the empty path, which stands for the current one.
fn parent_name(path: &Path) -> String {
    path.parent().map(|parent| if parent.as_os_str().is_empty() { Path::new(".") } else { parent }).and_then(real_name).unwrap_or_default()
}

/// For Apple Mail's `Name.mbox/mbox`, the mailbox's name `Name`.
fn apple_mailbox(path: &Path) -> Option<String> {
    let parent = parent_name(path);
    let stem = parent.strip_suffix(".mbox").filter(|stem| !stem.is_empty())?;
    (file_name(path) == "mbox").then(|| stem.to_string())
}

/// A PST file starts with the magic "!BDN" (MS-PST 2.2.2.6).
fn is_pst(path: &Path) -> io::Result<bool> {
    let mut magic = Vec::with_capacity(4);
    File::open(path)?.take(4).read_to_end(&mut magic)?;
    Ok(magic == b"!BDN")
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

    /// The cases the oracle's self-test (`tests/oracle/compare_mail.py`, `_date_rule_self_test`) holds too.
    const DATE_HEADER_LINES: [(&str, bool); 19] = [
        ("From a@example.invalid Sat, 01 Jan 2024 11:00:00 +0000", true),
        ("From a@example.invalid Mon, 1 Jan 2024 10:00:00 -0500", true),
        ("From a Sat, 01 Jan 2024 11:00:00", true),
        ("From a 1 Jan 2024 11:00 GMT", true),
        ("From a Sat, 01 Jan 2024 11:00:00 +0000 (UTC)", true),
        ("From a Mon, 1 Jan 2024 12:00:00 UTC", true),
        ("From a Mon, 1 Jan 2024 12:00:00 CET", true),
        ("From 9:00 to 17:00", false),
        ("From our website", false),
        ("From me 12:30 today", false),
        ("From here on 2024", false),
        ("From John Sat, 01 Jan 2024 11:00:00 +0000 wrote:", false),
        ("From a Sat, 31 Feb 2024 10:00:00 +0000", false),
        ("From a Mon Jan  1 10:00:00 2024 +0000", false),
        ("From a Sat, 01 Jan 2024 11:00:00 +2400", false),
        ("From a Sat,, 01 Jan 2024 11:00:00 +0000", false),
        ("From a Sat, +1 Jan 2024 11:00:00 +0000", false),
        ("From a Sat, 01 Jan 2024 11:00:00 gmt", false),
        ("From a Sat, 01 Jan 1899 11:00:00 +0000", false),
    ];

    /// The asctime cases the oracle's self-test holds too.
    const ASCTIME_LINES: [(&str, bool); 41] = [
        ("From a Mon Jan  1 10:00:00 2024", true),
        ("From - Mon Jan  1 10:00:00 2024", true),
        ("From 1@xxx Mon Jan 01 10:01:00 +0000 2024", true),
        ("From a Mon Jan  1 10:00:00 2024 +0000", true),
        ("From a Mon Jan  1 10:00:00 2024 GMT", true),
        ("From a Mon Jan  1 10:00:00 2024 remote from host", true),
        ("From me at 12:30 in 2024 too", false),
        ("From a : 2024", false),
        ("From a 1:2 2024", false),
        ("From a Mon Jan 1 10:0 2024", false),
        ("From a 10:00 -2024", false),
        ("From 9:00 to 17:00, 2024", false),
        ("From John Sat, 06 Jan 2024 11:00:00 +0000", false),
        ("From", false),
        // A zone after the year only in the order mail programs write: a month, then the time just before the year.
        ("From a Mon Jan  1 10:00 PST 2024", true),
        ("From 10:00 to 12:00 on 15 March 2024 CET", false),
        ("From 8:30 to 17:00 on 2 Jan 2024 GMT", false),
        ("From our office, 10:00 Jan 5 2024 PST", false),
        ("From noon 12:00 2024 +0100", false),
        ("From Monday 9:00 AM until Friday 5 January 2024 EST", false),
        // The same order is needed without a zone; real forms keep splitting.
        ("From 10:00 to 12:00 on 15 March 2024", false),
        ("From 8:30 to 17:00 on 2 Jan 2024", false),
        ("From our office, 10:00 Jan 5 2024", false),
        ("From 9:00 to 17:00 in 2024", false),
        ("From noon 12:00 2024", false),
        ("From me at 10:00 GMT 2024", false),
        ("From 10:00 to 12:00 PST 2024", false),
        ("From Monday 9:00 AM until Friday 5 January 2024", false),
        ("From u Mon Mar 30 10:00:00 MET DST 1998", true),
        ("From MAILER-DAEMON Mon Jan  1 10:00:00 2024", true),
        ("From ???@??? Mon Jan  1 10:00:00 2024", true),
        // System V's %Z: tzdata's numeric abbreviations and mixed-case names; a month name is never a zone.
        ("From user Mon Jan  1 10:00:00 +03 2024", true),
        ("From user Mon Jan  1 10:00:00 -03 2024", true),
        ("From user Mon Jan  1 10:00:00 ChST 2024", true),
        ("From a 10:00:00 2024 JAN", false),
        ("From x 9:00 2024 MAY", false),
        // A capitalised word is no zone abbreviation; one needs two capitals at least ("ChST").
        ("From now on you may reach me at 10:00 Ext 4711", false),
        ("From Mon Jan 6 at 10:00 Room 4711", false),
        ("From Tue Dec 3, 14:00 Gate 1234", false),
        ("From the May 5 meeting at 10:00 Room 2024", false),
        ("From us Dec 24 at 18:00 Merry Xmas 2024", false),
    ];

    #[test]
    fn date_header_style_reads_only_dates() {
        assert_eq!(date_header_style(b"From a@example.invalid Sat, 01 Jan 2024 11:00:00 +0000\n"), Some(1_704_106_800));
        assert_eq!(date_header_style(b"From a@example.invalid Mon, 1 Jan 2024 10:00:00 -0500\r\n"), Some(1_704_121_200));
        assert_eq!(date_header_style(b"From a 1 Jan 2024 11:00 GMT\n"), Some(1_704_106_800));
        for (line, accepted) in DATE_HEADER_LINES {
            assert_eq!(date_header_style(line.as_bytes()).is_some(), accepted, "{line:?}");
        }
    }

    #[test]
    fn from_line_dates() {
        let date = |line: &str| from_line_date(line.as_bytes());
        assert_eq!(date("From a Mon Jan  1 10:00:00 2024\n"), Some(Some(1_704_103_200)));
        assert_eq!(date("From a Mon Jan  1 10:00:00 2024 remote from host\n"), Some(Some(1_704_103_200)));
        assert_eq!(from_line_date(b"From j\xfcrgen@example.invalid Mon Jan  1 10:00:00 2024\n"), Some(Some(1_704_103_200)));
        // A zone after the year (UW c-client, Alpine) or before it (Google Takeout) gives the offset.
        assert_eq!(date("From a Mon Jan  1 10:00:00 2024 +0000\n"), Some(Some(1_704_103_200)));
        assert_eq!(date("From a Mon Jan  1 10:00:00 2024 -0500\n"), Some(Some(1_704_121_200)));
        assert_eq!(date("From a Mon Jan 01 10:00:00 -0500 2024\n"), Some(Some(1_704_121_200)));
        // A zone name just before the year (System V) gives its offset too.
        assert_eq!(date("From a Mon Jan  1 10:00:00 PST 2024\n"), Some(Some(1_704_132_000)));
        // tzdata's "+03"; of two zone words, the numeric one gives the offset.
        assert_eq!(date("From a Mon Jan  1 10:00:00 +03 2024\n"), Some(Some(1_704_092_400)));
        assert_eq!(date("From a Mon Jan  1 10:00:00 +0100 CET 2024\n"), Some(Some(1_704_099_600)));
        for (line, accepted) in ASCTIME_LINES {
            assert_eq!(from_line_date(line.as_bytes()).is_some(), accepted, "{line:?}");
        }
    }

    #[test]
    fn mbox_entries_split_at_every_kind_of_from_line_and_only_there() {
        let text = "From a Mon Jan  1 10:00:00 2024 +0000\nSubject: one\n\nbody\nFrom John Sat, 06 Jan 2024 11:00:00 +0000\n\n\
                    From a Mon Jan  1 11:00:00 2024 +0000\nSubject: two\n\nbody\n\n\
                    From a Mon, 1 Jan 2024 12:00:00 UTC\nSubject: three\n\nbody\n";
        let mut entries = MboxEntries { reader: io::Cursor::new(text.as_bytes()), line: Vec::new(), ahead: Vec::new(), pending: None, first: true };
        let mut dates = Vec::new();
        while let Some((raw, date)) = entries.next_entry().unwrap() {
            assert!(raw.starts_with(b"Subject: "), "{:?}", String::from_utf8_lossy(&raw));
            dates.push(date);
        }
        assert_eq!(dates, vec![Some(1_704_103_200), Some(1_704_106_800), Some(1_704_110_400)]);
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
