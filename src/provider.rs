//! Cloud provider trait (D8): the sync engine and `Store` talk to this
//! surface only, so OneDrive and LocalFolder (and future providers) are
//! interchangeable. Paths are always slash-separated and relative to the
//! provider's root (OneDrive app folder, or an arbitrary local directory).

use anyhow::Result;

pub trait Provider {
    fn exists(&self, rel: &str) -> Result<bool>;
    fn download(&self, rel: &str) -> Result<Vec<u8>>;
    fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&self, rel: &str) -> Result<()>;
}
