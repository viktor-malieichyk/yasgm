//! Cloud provider trait (D8): the sync engine and `Store` talk to this
//! surface only, so OneDrive and LocalFolder (and future providers) are
//! interchangeable. Paths are always slash-separated and relative to the
//! provider's root (OneDrive app folder, or an arbitrary local directory).

use anyhow::Result;

pub trait Provider: Send + Sync {
    fn exists(&self, rel: &str) -> Result<bool>;
    fn download(&self, rel: &str) -> Result<Vec<u8>>;
    fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&self, rel: &str) -> Result<()>;
    /// Names of the entries (files and folders) directly inside folder
    /// `rel`; empty if it doesn't exist.
    fn list(&self, rel: &str) -> Result<Vec<String>>;
    /// `download`, or None if `rel` doesn't exist. Providers where checking
    /// first costs a round trip override this.
    fn download_if_exists(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        if self.exists(rel)? { self.download(rel).map(Some) } else { Ok(None) }
    }
}
