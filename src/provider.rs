//! Cloud provider trait (D8): the sync engine and `Store` talk to this
//! surface only, so OneDrive and LocalFolder (and future providers) are
//! interchangeable. Paths are always slash-separated and relative to the
//! provider's root (OneDrive app folder, or an arbitrary local directory).

use anyhow::Result;
use sha2::{Digest, Sha256};

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
    /// `download_if_exists` plus a tag for this content version, for
    /// `upload_if`. Default: a hash of the content.
    fn download_tagged(&self, rel: &str) -> Result<Option<(Vec<u8>, String)>> {
        Ok(self.download_if_exists(rel)?.map(|bytes| {
            let tag = content_tag(&bytes);
            (bytes, tag)
        }))
    }
    /// Writes `bytes` only if `rel` is still what `download_tagged` returned
    /// as `expected` (None: only if it doesn't exist yet). `Ok(false)` when
    /// someone else changed it first — re-read and try again. The default
    /// compares and then writes, which narrows the window but can't close
    /// it; OneDrive overrides it with a real conditional request.
    fn upload_if(&self, rel: &str, bytes: &[u8], expected: Option<&str>) -> Result<bool> {
        let current = self.download_if_exists(rel)?.map(|b| content_tag(&b));
        if current.as_deref() != expected {
            return Ok(false);
        }
        self.upload(rel, bytes)?;
        Ok(true)
    }
}

fn content_tag(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The provider couldn't be reached at all (no network, DNS, the sign-in
/// service unreachable) — as opposed to answering with an error. Only this
/// counts as "offline": the cloud is then treated as having nothing new and
/// backups go to the pending store (D18); any other failure is reported.
#[derive(Debug)]
pub struct Offline(pub String);

impl std::fmt::Display for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cloud unreachable: {}", self.0)
    }
}

impl std::error::Error for Offline {}

pub fn is_offline(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<Offline>())
}
