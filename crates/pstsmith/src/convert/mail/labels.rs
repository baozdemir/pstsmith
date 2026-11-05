//! Gmail labels (the `X-Gmail-Labels` header of a Google Takeout MBOX).
//!
//! Takeout writes every message once, with all its labels. A message is written once too: its folder is the first of
//! its system folders, in the order of [`SYSTEM_FOLDERS`], named as Outlook names its default folders, else "Archive";
//! "Unread" sets the read state; "Starred" is kept as a category; the labels that only mirror Gmail's own views are
//! dropped; every other label becomes a category.

/// Gmail's system labels that decide the folder, and the Outlook default folder each one becomes.
pub(crate) const SYSTEM_FOLDERS: &[(&str, &str)] =
    &[("Inbox", "Inbox"), ("Sent", "Sent Items"), ("Drafts", "Drafts"), ("Spam", "Junk Email"), ("Trash", "Deleted Items")];

/// The folder of a labelled message that has none of the system folder labels.
pub(crate) const ARCHIVE_FOLDER: &str = "Archive";

/// Labels that only mirror Gmail's views and carry nothing the message does not already say.
const DROPPED: &[&str] = &["Archived", "Important", "Opened"];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Labels {
    /// The folder the message is written to.
    pub(crate) folder: String,
    /// "Unread" was among the labels.
    pub(crate) unread: bool,
    /// "Drafts" was among the labels.
    pub(crate) draft: bool,
    /// The labels kept as categories, in header order, each once.
    pub(crate) categories: Vec<String>,
}

/// The labels of a decoded `X-Gmail-Labels` value: comma-separated, a label containing a comma in double quotes.
fn split(value: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in value.chars() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => labels.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    labels.push(current);
    labels.into_iter().map(|label| label.trim().to_string()).filter(|label| !label.is_empty()).collect()
}

pub(crate) fn parse(value: &str) -> Labels {
    let labels = split(value);
    let folder = SYSTEM_FOLDERS
        .iter()
        .find(|(label, _)| labels.iter().any(|given| given == label))
        .map_or(ARCHIVE_FOLDER, |(_, folder)| folder);
    let mut categories: Vec<String> = Vec::new();
    for label in &labels {
        let system = SYSTEM_FOLDERS.iter().any(|(name, _)| name == label);
        let tab = label.starts_with("Category ");
        if system || tab || label == "Unread" || DROPPED.contains(&label.as_str()) || categories.contains(label) {
            continue;
        }
        categories.push(label.clone());
    }
    Labels {
        folder: folder.to_string(),
        unread: labels.iter().any(|label| label == "Unread"),
        draft: labels.iter().any(|label| label == "Drafts"),
        categories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_labels_rules() {
        let labels = parse(r#"Inbox,Important,Unread,Category Updates,Starred,"Projects, 2026",Müşteri,Opened"#);
        assert_eq!(labels.folder, "Inbox");
        assert!(labels.unread);
        assert!(!labels.draft);
        assert_eq!(labels.categories, vec!["Starred", "Projects, 2026", "Müşteri"]);
    }

    #[test]
    fn gmail_folder_is_the_first_system_label_in_outlook_order() {
        assert_eq!(parse("Trash,Sent").folder, "Sent Items");
        assert_eq!(parse("Spam").folder, "Junk Email");
        assert_eq!(parse("Trash").folder, "Deleted Items");
        let draft = parse("Drafts,Opened");
        assert_eq!(draft.folder, "Drafts");
        assert!(draft.draft);
    }

    #[test]
    fn gmail_message_without_a_system_label_is_archived() {
        let labels = parse("Archived,Receipts,Receipts");
        assert_eq!(labels.folder, "Archive");
        assert!(!labels.unread);
        assert_eq!(labels.categories, vec!["Receipts"]);
    }
}
