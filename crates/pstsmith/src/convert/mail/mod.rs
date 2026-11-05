//! Mail input: EML, MBOX and Maildir messages mapped to manifest items.
//!
//! A message is mapped to the manifest's item form (`pst-export-manifest/2`) following MS-OXCMAIL, the specification
//! of the conversion of a MIME message to a Message object, and then takes the manifest conversion's path unchanged.

mod labels;
pub(crate) mod mime;
#[cfg(test)]
mod mime_tests;
mod report;

use sha2::{Digest, Sha256};
use std::io;

/// Where a message comes from, for its search key and its times.
#[derive(Clone, Debug, Default)]
pub(crate) struct Origin {
    /// The container's name as given (a file or folder name, `/`-separated, never an absolute path).
    pub(crate) container: String,
    /// The message's position in its container, from 1.
    pub(crate) ordinal: u64,
    /// The date of the MBOX "From " line, Unix seconds.
    pub(crate) mbox_date: Option<i64>,
    /// Maildir flags: `Some(seen)` when the source states the read state.
    pub(crate) seen: Option<bool>,
    /// Maildir draft flag.
    pub(crate) draft: bool,
}

/// A file staged for the manifest: the path the manifest names, its size and its SHA-256 (hexadecimal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

/// Where attachment bytes go before the manifest conversion reads them back.
pub(crate) trait Stage {
    fn stage(&mut self, data: &[u8]) -> io::Result<Staged>;
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

#[cfg(test)]
pub(crate) mod test_stage {
    use super::{sha256_hex, Stage, Staged};
    use std::io;

    /// Keeps staged files in memory; their paths are `mem:N`.
    #[derive(Default)]
    pub(crate) struct MemoryStage {
        pub(crate) files: Vec<Vec<u8>>,
    }

    impl Stage for MemoryStage {
        fn stage(&mut self, data: &[u8]) -> io::Result<Staged> {
            self.files.push(data.to_vec());
            Ok(Staged { path: format!("mem:{}", self.files.len() - 1), size: data.len() as u64, sha256: sha256_hex(data) })
        }
    }
}
