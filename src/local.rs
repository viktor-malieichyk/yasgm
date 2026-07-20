//! LocalFolder cloud provider (Phase 4, D8): point the sync engine at any
//! directory instead of OneDrive — e.g. a folder managed by Syncthing or a
//! desktop cloud-sync client. This is exactly the setup discussed under
//! "Local data location" in DESIGN.md; the caveats documented there
//! (double-sync hazard, no completion signal, partial reads, conflict-copy
//! litter) are the user's to manage — this provider just needs to behave
//! well as one participant in that folder.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::provider::Provider;

pub struct LocalFolderProvider {
    root: PathBuf,
}

impl LocalFolderProvider {
    pub fn new(root: PathBuf) -> LocalFolderProvider {
        LocalFolderProvider { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl Provider for LocalFolderProvider {
    fn exists(&self, rel: &str) -> Result<bool> {
        Ok(self.path(rel).exists())
    }

    fn download(&self, rel: &str) -> Result<Vec<u8>> {
        fs::read(self.path(rel)).with_context(|| format!("reading {rel}"))
    }

    /// Write to a sibling temp file and rename into place, so anything else
    /// watching this folder (a desktop sync client, another `yasgm`
    /// instance) never observes a partially written file.
    fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let dest = self.path(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut tmp_name = dest.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(format!(".tmp{}", std::process::id()));
        let tmp = dest.with_file_name(tmp_name);
        fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &dest).with_context(|| format!("renaming into {}", dest.display()))
    }

    fn delete(&self, rel: &str) -> Result<()> {
        match fs::remove_file(self.path(rel)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("deleting {rel}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yasgm-local-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn upload_download_roundtrip_through_nested_path() {
        let root = scratch("roundtrip");
        let p = LocalFolderProvider::new(root.clone());
        assert!(!p.exists("accounts/1/games/2/index.json").unwrap());
        p.upload("accounts/1/games/2/index.json", b"hello").unwrap();
        assert!(p.exists("accounts/1/games/2/index.json").unwrap());
        assert_eq!(p.download("accounts/1/games/2/index.json").unwrap(), b"hello");
        // No leftover temp file from the write-then-rename.
        let leftovers: Vec<_> = fs::read_dir(root.join("accounts/1/games/2"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upload_overwrites_existing_content() {
        let root = scratch("overwrite");
        let p = LocalFolderProvider::new(root.clone());
        p.upload("v.zip", b"v1").unwrap();
        p.upload("v.zip", b"v2 longer content").unwrap();
        assert_eq!(p.download("v.zip").unwrap(), b"v2 longer content");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_is_idempotent() {
        let root = scratch("delete");
        let p = LocalFolderProvider::new(root.clone());
        p.upload("v.zip", b"x").unwrap();
        p.delete("v.zip").unwrap();
        assert!(!p.exists("v.zip").unwrap());
        p.delete("v.zip").unwrap(); // deleting again must not error
        fs::remove_dir_all(&root).unwrap();
    }
}
