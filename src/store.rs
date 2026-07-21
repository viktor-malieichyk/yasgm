//! Cloud snapshot store, with an automatic local fallback for offline use.
//!
//! Layout (per D4, per Steam account), on both the primary provider and the
//! local pending fallback below:
//!   accounts/<steam-account-id>/games/<appid>/index.json
//!   accounts/<steam-account-id>/games/<appid>/versions/<version-id>.zip
//!
//! Versions are immutable. The **active head** is the newest *non-pinned*
//! version; pinned versions are preserved archives (conflict losers and
//! pre-restore states, D5/D14) that never compete for "latest" and are never
//! pruned automatically (retention keeps the newest N non-pinned, D9).
//!
//! **Offline fallback**: every `Store` also owns a `pending` provider — a
//! `LocalFolderProvider` rooted at a fixed local directory
//! (`pending_dir()`), independent of whatever the user configured as their
//! primary provider (D8). If a push to the primary provider fails for any
//! reason (network down, expired auth, quota — anything), the snapshot is
//! captured locally instead so the save is still versioned; `load_index`
//! transparently merges both providers' indices (tagging pending entries
//! via `Version::pending`, an in-memory-only flag never persisted to either
//! index.json) so pending versions are visible and restorable everywhere
//! versions normally are. Every push first calls `flush_pending`, which
//! opportunistically retries uploading any stranded pending versions to the
//! primary provider — the moment connectivity returns, the next sync/backup
//! run catches up automatically, with no separate "reconnect" step needed.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::local::LocalFolderProvider;
use crate::provider::Provider;
use crate::snapshot::{Mount, Snapshot};

pub const DEFAULT_KEEP: usize = 10;
pub const SIZE_WARN_BYTES: u64 = 1_073_741_824; // 1 GB (D9)

#[derive(Debug, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub game: String,
    pub app_id: u64,
    #[serde(default)]
    pub versions: Vec<Version>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub created: u64,
    pub machine: String,
    pub os: String,
    pub content_hash: String,
    pub size: u64,
    pub files: u64,
    #[serde(default)]
    pub pinned: bool,
    pub mounts: Vec<Mount>,
    /// Set only in-memory by `Store::load_index`'s merge step — never read
    /// from or written to either provider's own index.json. True means this
    /// version currently lives in the local offline-pending store rather
    /// than the primary provider.
    #[serde(default, skip_serializing)]
    pub pending: bool,
}

pub struct Store {
    primary: Box<dyn Provider>,
    pending: Box<dyn Provider>,
    account: String,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn timestamp_id(secs: u64) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

pub fn machine_name() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let name: String = raw
        .trim()
        .trim_end_matches(".local")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    if name.is_empty() { "machine".to_owned() } else { name }
}

/// Fixed (not user-configurable) local fallback location for versions that
/// couldn't reach the primary provider — always outside any cloud-synced
/// folder since it's not somewhere the user points a sync client at (D13).
fn pending_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data directory on this platform")?
        .join("yasgm/pending");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

impl Store {
    pub fn new(primary: Box<dyn Provider>, account: String) -> Result<Store> {
        let pending: Box<dyn Provider> = Box::new(LocalFolderProvider::new(pending_dir()?));
        Ok(Store { primary, pending, account })
    }

    #[cfg(test)]
    fn with_providers(primary: Box<dyn Provider>, pending: Box<dyn Provider>, account: String) -> Store {
        Store { primary, pending, account }
    }

    fn game_base(&self, app_id: u64) -> String {
        format!("accounts/{}/games/{}", self.account, app_id)
    }

    fn load_index_from(&self, provider: &dyn Provider, app_id: u64) -> Result<Option<Index>> {
        let rel = format!("{}/index.json", self.game_base(app_id));
        if !provider.exists(&rel)? {
            return Ok(None);
        }
        let bytes = provider.download(&rel)?;
        Ok(Some(serde_json::from_slice(&bytes).context("parsing index.json")?))
    }

    fn save_index_to(&self, provider: &dyn Provider, index: &Index) -> Result<()> {
        let rel = format!("{}/index.json", self.game_base(index.app_id));
        provider.upload(&rel, &serde_json::to_vec_pretty(index)?)
    }

    /// Merged view across the primary provider and any local pending
    /// (offline-fallback) versions, so callers never need to know which
    /// provider a version actually lives on.
    pub fn load_index(&self, app_id: u64) -> Result<Option<Index>> {
        // An unreachable primary shouldn't hide pending versions — treat it
        // as "nothing from primary right now" rather than a hard error. A
        // broken *local* pending store is a real problem, so that error
        // still propagates.
        let primary = self.load_index_from(&*self.primary, app_id).unwrap_or(None);
        let pending = self.load_index_from(&*self.pending, app_id)?;
        Ok(match (primary, pending) {
            (None, None) => None,
            (Some(idx), None) => Some(idx),
            (None, Some(mut idx)) => {
                for v in &mut idx.versions {
                    v.pending = true;
                }
                Some(idx)
            }
            (Some(mut idx), Some(mut pend)) => {
                for v in &mut pend.versions {
                    v.pending = true;
                }
                idx.versions.extend(pend.versions);
                Some(idx)
            }
        })
    }

    /// Active head: newest non-pinned version, regardless of whether it's on
    /// the primary provider or still pending locally.
    pub fn head(index: &Index) -> Option<&Version> {
        index
            .versions
            .iter()
            .filter(|v| !v.pinned)
            .max_by_key(|v| v.created)
    }

    /// Upload a captured snapshot as a new version. Consumes the staging zip.
    pub fn push(
        &self,
        game_name: &str,
        app_id: u64,
        snapshot: &Snapshot,
        os: &str,
        keep: usize,
        pinned: bool,
    ) -> Result<Version> {
        let bytes = fs::read(&snapshot.zip_path).context("reading staged zip")?;
        let version = self.push_raw(
            game_name,
            app_id,
            &bytes,
            &snapshot.content_hash,
            snapshot.file_count,
            snapshot.mounts.clone(),
            os,
            keep,
            pinned,
        )?;
        let _ = fs::remove_file(&snapshot.zip_path);
        Ok(version)
    }

    /// Upload raw zip bytes as a new version (used by push and by restore's
    /// "make this content the active head again" step). Tries the primary
    /// provider first; on any failure, falls back to the local pending
    /// store so the save is still versioned somewhere.
    #[allow(clippy::too_many_arguments)]
    pub fn push_raw(
        &self,
        game_name: &str,
        app_id: u64,
        zip_bytes: &[u8],
        content_hash: &str,
        file_count: u64,
        mounts: Vec<Mount>,
        os: &str,
        keep: usize,
        pinned: bool,
    ) -> Result<Version> {
        // Best-effort catch-up: if we can reach the primary now, clear out
        // anything stranded from a previous offline run before adding to
        // it. Failure here just means we're still offline — proceed either
        // way, since the fallback path below covers that.
        let _ = self.flush_pending(app_id, keep);

        match self.push_raw_to(&*self.primary, game_name, app_id, zip_bytes, content_hash, file_count, mounts.clone(), os, keep, pinned)
        {
            Ok(version) => Ok(version),
            Err(primary_err) => {
                let mut version = self
                    .push_raw_to(&*self.pending, game_name, app_id, zip_bytes, content_hash, file_count, mounts, os, keep, pinned)
                    .with_context(|| format!("also failed writing local pending backup after primary failed: {primary_err:#}"))?;
                version.pending = true;
                eprintln!(
                    "{game_name}: couldn't reach the cloud ({primary_err:#}) — saved a local \
                     pending backup instead; it'll sync automatically next time this game backs up"
                );
                Ok(version)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_raw_to(
        &self,
        provider: &dyn Provider,
        game_name: &str,
        app_id: u64,
        zip_bytes: &[u8],
        content_hash: &str,
        file_count: u64,
        mounts: Vec<Mount>,
        os: &str,
        keep: usize,
        pinned: bool,
    ) -> Result<Version> {
        let created = unix_now();
        let mut id = format!("{}_{}", timestamp_id(created), machine_name());

        let mut index = self.load_index_from(provider, app_id)?.unwrap_or(Index {
            schema: 1,
            game: game_name.to_owned(),
            app_id,
            versions: Vec::new(),
        });
        while index.versions.iter().any(|v| v.id == id) {
            id.push('x'); // same machine, same second — keep ids unique
        }
        let version = Version {
            id: id.clone(),
            created,
            machine: machine_name(),
            os: os.to_owned(),
            content_hash: content_hash.to_owned(),
            size: zip_bytes.len() as u64,
            files: file_count,
            pinned,
            mounts,
            pending: false,
        };

        let zip_rel = format!("{}/versions/{id}.zip", self.game_base(app_id));
        provider.upload(&zip_rel, zip_bytes)?;
        index.versions.push(version.clone());
        self.apply_retention_to(provider, &mut index, keep)?;
        self.save_index_to(provider, &index)?;

        let total: u64 = index.versions.iter().map(|v| v.size).sum();
        if total > SIZE_WARN_BYTES {
            eprintln!(
                "warning: {game_name} now stores {:.1} GB (D9 threshold is 1 GB); \
                 consider lowering its version count (`yasgm config {app_id} --keep N`)",
                total as f64 / 1_073_741_824.0
            );
        }
        Ok(version)
    }

    /// Move any versions sitting in the local pending store into the
    /// primary provider now that it's (hopefully) reachable again. Returns
    /// how many were flushed (0 if there was nothing pending). Public and
    /// meant to be called explicitly at the start of `sync`/`backup` — not
    /// just relied on via `push_raw`'s own internal call — because a game
    /// whose save content hasn't changed since a prior offline backup never
    /// reaches `push_raw` at all (callers short-circuit on "unchanged"), so
    /// without an explicit call a pending version could sit stranded
    /// indefinitely even after connectivity returns. Leaves no
    /// partially-flushed state: a version is only deleted from pending
    /// after it's confirmed present on primary.
    pub fn flush_pending(&self, app_id: u64, keep: usize) -> Result<usize> {
        let Some(pending_index) = self.load_index_from(&*self.pending, app_id)? else {
            return Ok(0);
        };
        if pending_index.versions.is_empty() {
            return Ok(0);
        }
        for version in &pending_index.versions {
            let zip_rel = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
            let bytes = self.pending.download(&zip_rel)?;
            self.push_raw_to(
                &*self.primary,
                &pending_index.game,
                app_id,
                &bytes,
                &version.content_hash,
                version.files,
                version.mounts.clone(),
                &version.os,
                keep,
                version.pinned,
            )?;
            self.pending.delete(&zip_rel)?;
        }
        let index_rel = format!("{}/index.json", self.game_base(app_id));
        self.pending.delete(&index_rel)?;
        Ok(pending_index.versions.len())
    }

    /// Keep the newest `keep` non-pinned versions; pinned ones always stay.
    fn apply_retention_to(&self, provider: &dyn Provider, index: &mut Index, keep: usize) -> Result<()> {
        let mut unpinned: Vec<(u64, String)> = index
            .versions
            .iter()
            .filter(|v| !v.pinned)
            .map(|v| (v.created, v.id.clone()))
            .collect();
        if unpinned.len() <= keep {
            return Ok(());
        }
        unpinned.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        let doomed: Vec<String> = unpinned.split_off(keep).into_iter().map(|(_, id)| id).collect();
        for id in &doomed {
            let zip_rel = format!("{}/versions/{id}.zip", self.game_base(index.app_id));
            provider.delete(&zip_rel)?;
        }
        index.versions.retain(|v| !doomed.contains(&v.id));
        Ok(())
    }

    /// Finds which provider (primary or pending) currently holds a version.
    fn locate(&self, app_id: u64, version_id: &str) -> Result<(&dyn Provider, Index)> {
        // An unreachable primary here just means "not found there" — still
        // check pending rather than treating it as a hard failure; only
        // bail if the version genuinely isn't on either.
        if let Ok(Some(index)) = self.load_index_from(&*self.primary, app_id) {
            if index.versions.iter().any(|v| v.id == version_id) {
                return Ok((&*self.primary, index));
            }
        }
        if let Some(index) = self.load_index_from(&*self.pending, app_id)? {
            if index.versions.iter().any(|v| v.id == version_id) {
                return Ok((&*self.pending, index));
            }
        }
        anyhow::bail!("version {version_id} not found")
    }

    pub fn set_pinned(&self, app_id: u64, version_id: &str, pinned: bool) -> Result<()> {
        let (provider, mut index) = self.locate(app_id, version_id)?;
        let version = index
            .versions
            .iter_mut()
            .find(|v| v.id == version_id)
            .expect("locate just confirmed this version is present");
        version.pinned = pinned;
        self.save_index_to(provider, &index)
    }

    /// Manual deletion (D5: pinned conflict versions live until the user
    /// deletes them).
    pub fn remove_version(&self, app_id: u64, version_id: &str) -> Result<()> {
        let (provider, mut index) = self.locate(app_id, version_id)?;
        let zip_rel = format!("{}/versions/{version_id}.zip", self.game_base(app_id));
        provider.delete(&zip_rel)?;
        index.versions.retain(|v| v.id != version_id);
        self.save_index_to(provider, &index)
    }

    pub fn download_version(&self, app_id: u64, version: &Version) -> Result<Vec<u8>> {
        let provider: &dyn Provider = if version.pending { &*self.pending } else { &*self.primary };
        let zip_rel = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
        provider.download(&zip_rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider that always fails, simulating "primary unreachable"
    /// without needing a real network outage.
    struct AlwaysFailProvider;

    impl Provider for AlwaysFailProvider {
        fn exists(&self, _rel: &str) -> Result<bool> {
            anyhow::bail!("simulated offline")
        }
        fn download(&self, _rel: &str) -> Result<Vec<u8>> {
            anyhow::bail!("simulated offline")
        }
        fn upload(&self, _rel: &str, _bytes: &[u8]) -> Result<()> {
            anyhow::bail!("simulated offline")
        }
        fn delete(&self, _rel: &str) -> Result<()> {
            anyhow::bail!("simulated offline")
        }
    }

    static SCRATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn scratch_dir(name: &str) -> PathBuf {
        let n = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yasgm-store-test-{name}-{n}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn mounts() -> Vec<Mount> {
        Vec::new()
    }

    #[test]
    fn push_falls_back_to_pending_when_primary_unreachable() {
        let pending_root = scratch_dir("pending");
        let store = Store::with_providers(
            Box::new(AlwaysFailProvider),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );

        let version = store
            .push_raw("Game", 42, b"zip-bytes", "hash1", 1, mounts(), "mac", 10, false)
            .expect("falls back to pending instead of erroring");
        assert!(version.pending, "version pushed via fallback should be marked pending");

        let index = store.load_index(42).unwrap().expect("index should exist");
        assert_eq!(index.versions.len(), 1);
        assert!(index.versions[0].pending);
        assert_eq!(Store::head(&index).unwrap().id, version.id);

        let bytes = store.download_version(42, &version).unwrap();
        assert_eq!(bytes, b"zip-bytes");

        fs::remove_dir_all(&pending_root).ok();
    }

    #[test]
    fn flush_moves_pending_versions_into_primary_on_next_push() {
        let primary_root = scratch_dir("primary");
        let pending_root = scratch_dir("pending");
        let primary = LocalFolderProvider::new(primary_root.clone());
        // Seed a pending version directly (as if an earlier offline push had
        // happened), bypassing push_raw so this test only exercises flush.
        {
            let seed_store = Store::with_providers(
                Box::new(AlwaysFailProvider),
                Box::new(LocalFolderProvider::new(pending_root.clone())),
                "acct".to_owned(),
            );
            seed_store
                .push_raw("Game", 7, b"offline-bytes", "hash-offline", 1, mounts(), "mac", 10, false)
                .unwrap();
        }

        let store = Store::with_providers(
            Box::new(primary),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        // A normal push against a now-reachable primary should flush the
        // stranded pending version first.
        let new_version = store
            .push_raw("Game", 7, b"online-bytes", "hash-online", 1, mounts(), "mac", 10, false)
            .unwrap();
        assert!(!new_version.pending);

        let index = store.load_index(7).unwrap().unwrap();
        assert_eq!(index.versions.len(), 2, "flushed version + new push");
        assert!(index.versions.iter().all(|v| !v.pending), "nothing should remain pending");
        assert!(index.versions.iter().any(|v| v.content_hash == "hash-offline"));
        assert!(index.versions.iter().any(|v| v.content_hash == "hash-online"));

        fs::remove_dir_all(&primary_root).ok();
        fs::remove_dir_all(&pending_root).ok();
    }

    #[test]
    fn set_pinned_and_remove_version_operate_on_whichever_store_holds_it() {
        let pending_root = scratch_dir("pinrm-pending");
        let store = Store::with_providers(
            Box::new(AlwaysFailProvider),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        let version = store
            .push_raw("Game", 1, b"data", "hash", 1, mounts(), "mac", 10, false)
            .unwrap();

        store.set_pinned(1, &version.id, true).unwrap();
        let index = store.load_index(1).unwrap().unwrap();
        assert!(index.versions[0].pinned);

        store.remove_version(1, &version.id).unwrap();
        let index = store.load_index(1).unwrap();
        assert!(index.is_none_or(|i| i.versions.is_empty()));

        fs::remove_dir_all(&pending_root).ok();
    }
}
