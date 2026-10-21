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
    /// When the saves were last written: the newest file modification time
    /// in the snapshot (unix seconds). Decides which side of a conflict
    /// becomes active (D5). Missing on versions made before 2026-10-22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<u64>,
    /// Set only in-memory by `Store::load_index`'s merge step — never read
    /// from or written to either provider's own index.json. True means this
    /// version currently lives in the local offline-pending store rather
    /// than the primary provider.
    #[serde(default, skip_serializing)]
    pub pending: bool,
}

/// The content of a version to store (everything but its id and date).
pub struct NewVersion<'a> {
    pub game: &'a str,
    pub app_id: u64,
    pub bytes: &'a [u8],
    pub content_hash: &'a str,
    pub files: u64,
    pub mounts: Vec<Mount>,
    pub os: &'a str,
    pub saved_at: Option<u64>,
    pub pinned: bool,
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

/// This device's name for version ids and the "from <machine>" labels.
/// Asked of the OS directly: SteamOS has no `hostname` program, so every
/// Deck used to be called "machine".
pub fn machine_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| sanitize_machine_name(&raw_machine_name())).clone()
}

fn raw_machine_name() -> String {
    if cfg!(windows) {
        return std::env::var("COMPUTERNAME").unwrap_or_default();
    }
    // Linux keeps it here; macOS has no such files and falls through.
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(name) = fs::read_to_string(path) {
            if !name.trim().is_empty() {
                return name;
            }
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

fn sanitize_machine_name(raw: &str) -> String {
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
pub fn pending_dir() -> Result<PathBuf> {
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
    pub(crate) fn with_providers(primary: Box<dyn Provider>, pending: Box<dyn Provider>, account: String) -> Store {
        Store { primary, pending, account }
    }

    fn game_base(&self, app_id: u64) -> String {
        format!("accounts/{}/games/{}", self.account, app_id)
    }

    fn index_rel(&self, app_id: u64) -> String {
        format!("{}/index.json", self.game_base(app_id))
    }

    fn load_index_from(&self, provider: &dyn Provider, app_id: u64) -> Result<Option<Index>> {
        Ok(self.load_index_tagged(provider, app_id)?.map(|(index, _)| index))
    }

    fn load_index_tagged(&self, provider: &dyn Provider, app_id: u64) -> Result<Option<(Index, String)>> {
        match provider.download_tagged(&self.index_rel(app_id))? {
            Some((bytes, tag)) => Ok(Some((serde_json::from_slice(&bytes).context("parsing index.json")?, tag))),
            None => Ok(None),
        }
    }

    /// Read-modify-write of one game's index on `provider`. Another machine
    /// may write the same index in between (two devices syncing a game at
    /// once); the write is conditional, and on a clash this re-reads and
    /// re-applies `change` to the newer index — so `change` must be
    /// repeatable. A game without an index starts from an empty one.
    fn update_index<T>(
        &self,
        provider: &dyn Provider,
        app_id: u64,
        game: &str,
        mut change: impl FnMut(&mut Index) -> Result<T>,
    ) -> Result<(T, Index)> {
        const ATTEMPTS: u64 = 5;
        for attempt in 1..=ATTEMPTS {
            let (mut index, tag) = match self.load_index_tagged(provider, app_id)? {
                Some((index, tag)) => (index, Some(tag)),
                None => (Index { schema: 1, game: game.to_owned(), app_id, versions: Vec::new() }, None),
            };
            let out = change(&mut index)?;
            if provider.upload_if(&self.index_rel(app_id), &serde_json::to_vec_pretty(&index)?, tag.as_deref())? {
                return Ok((out, index));
            }
            std::thread::sleep(std::time::Duration::from_millis(150 * attempt));
        }
        anyhow::bail!("{game}: its version list kept changing on another device — try again")
    }

    /// Steam accounts with a folder in the cloud, besides this store's one,
    /// and how many games each has.
    pub fn other_accounts(&self) -> Result<Vec<(String, usize)>> {
        let mut out = Vec::new();
        for account in self.primary.list("accounts")? {
            if account == self.account {
                continue;
            }
            let games = self.primary.list(&format!("accounts/{account}/games"))?;
            out.push((account, games.iter().filter(|g| g.parse::<u64>().is_ok()).count()));
        }
        out.sort();
        Ok(out)
    }

    /// Every game with saves in the cloud for this account (or stranded in
    /// the local pending store), including games not installed here.
    pub fn cloud_app_ids(&self) -> Result<Vec<u64>> {
        let base = format!("accounts/{}/games", self.account);
        let mut ids: Vec<u64> = self
            .primary
            .list(&base)?
            .into_iter()
            .chain(self.pending.list(&base).unwrap_or_default())
            .filter_map(|name| name.parse().ok())
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Merged view across the primary provider and any local pending
    /// (offline-fallback) versions, so callers never need to know which
    /// provider a version actually lives on.
    pub fn load_index(&self, app_id: u64) -> Result<Option<Index>> {
        // An unreachable primary shouldn't hide pending versions — offline
        // counts as "nothing from primary right now". Any other failure (a
        // rejected request, a damaged index) is reported: treating it as
        // "no saves in the cloud" would show empty version lists and send
        // sync down its first-upload path.
        let primary = match self.load_index_from(&*self.primary, app_id) {
            Ok(index) => index,
            Err(err) if crate::provider::is_offline(&err) => None,
            Err(err) => return Err(err),
        };
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
            &NewVersion {
                game: game_name,
                app_id,
                bytes: &bytes,
                content_hash: &snapshot.content_hash,
                files: snapshot.file_count,
                mounts: snapshot.mounts.clone(),
                os,
                saved_at: snapshot.saved_at,
                pinned,
            },
            keep,
        )?;
        let _ = fs::remove_file(&snapshot.zip_path);
        Ok(version)
    }

    /// Upload raw zip bytes as a new version (used by push and by restore's
    /// "make this content the active head again" step). Tries the primary
    /// provider first; on any failure, falls back to the local pending
    /// store so the save is still versioned somewhere.
    pub fn push_raw(&self, new: &NewVersion, keep: usize) -> Result<Version> {
        // Best-effort catch-up: if we can reach the primary now, clear out
        // anything stranded from a previous offline run before adding to
        // it. Failure here just means we're still offline — proceed either
        // way, since the fallback path below covers that.
        let _ = self.flush_pending(new.app_id, keep);

        match self.push_raw_to(&*self.primary, new, keep, None) {
            Ok(version) => Ok(version),
            Err(primary_err) => {
                let mut version = self
                    .push_raw_to(&*self.pending, new, keep, None)
                    .with_context(|| format!("also failed writing local pending backup after primary failed: {primary_err:#}"))?;
                version.pending = true;
                eprintln!(
                    "{}: couldn't reach the cloud ({primary_err:#}) — saved a local \
                     pending backup instead; it'll sync automatically next time this game backs up",
                    new.game
                );
                Ok(version)
            }
        }
    }

    /// Stores `new` on `provider`. `existing` keeps a version's identity (id,
    /// date, machine) when it moves from the pending store to the primary,
    /// so a backup made offline keeps its place in time instead of looking
    /// like the newest version.
    fn push_raw_to(&self, provider: &dyn Provider, new: &NewVersion, keep: usize, existing: Option<&Version>) -> Result<Version> {
        let app_id = new.app_id;
        let current = self.load_index_from(provider, app_id)?.map(|i| i.versions).unwrap_or_default();
        let unique = |mut id: String| {
            while current.iter().any(|o| o.id == id) {
                id.push('x'); // same machine, same second — keep ids unique
            }
            id
        };
        let version = match existing {
            Some(v) => {
                // Already there: an earlier flush stored it but stopped
                // before clearing it from the pending store.
                if current.iter().any(|o| o.id == v.id && o.content_hash == v.content_hash) {
                    return Ok(Version { pending: false, ..v.clone() });
                }
                Version { id: unique(v.id.clone()), pending: false, ..v.clone() }
            }
            None => {
                let created = unix_now();
                Version {
                    id: unique(format!("{}_{}", timestamp_id(created), machine_name())),
                    created,
                    machine: machine_name(),
                    os: new.os.to_owned(),
                    content_hash: new.content_hash.to_owned(),
                    size: new.bytes.len() as u64,
                    files: new.files,
                    pinned: new.pinned,
                    mounts: new.mounts.clone(),
                    saved_at: new.saved_at,
                    pending: false,
                }
            }
        };

        let zip_rel = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
        provider.upload(&zip_rel, new.bytes)?;
        // Index first, then the pruned zips: a failure in between leaves
        // unreferenced zips, never index entries without their zip.
        let (doomed, index) = self.update_index(provider, app_id, new.game, |index| {
            if index.versions.iter().any(|v| v.id == version.id) {
                anyhow::bail!("version id {} was taken meanwhile", version.id);
            }
            index.versions.push(version.clone());
            Ok(Self::apply_retention(index, keep))
        })?;
        for id in &doomed {
            let _ = provider.delete(&format!("{}/versions/{id}.zip", self.game_base(app_id)));
        }

        let total: u64 = index.versions.iter().map(|v| v.size).sum();
        if total > SIZE_WARN_BYTES {
            eprintln!(
                "warning: {} now stores {:.1} GB (D9 threshold is 1 GB); \
                 consider lowering its version count (`yasgm config {app_id} --keep N`)",
                new.game,
                total as f64 / 1_073_741_824.0
            );
        }
        Ok(version)
    }

    /// Moves versions from the local pending store into the primary
    /// provider now that it's (hopefully) reachable again, keeping each
    /// one's id and date. Returns the ids moved. Called at the start of
    /// every sync/backup, not only from `push_raw`: a game whose saves
    /// haven't changed since an offline backup never reaches `push_raw`.
    /// Progress is recorded after each version, so stopping part-way (the
    /// network drops again) leaves both stores consistent.
    pub fn flush_pending(&self, app_id: u64, keep: usize) -> Result<Vec<String>> {
        let Some(mut pending_index) = self.load_index_from(&*self.pending, app_id)? else {
            return Ok(Vec::new());
        };
        let index_rel = self.index_rel(app_id);
        let mut flushed = Vec::new();
        while let Some(version) = pending_index.versions.first().cloned() {
            let zip_rel = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
            let bytes = self.pending.download(&zip_rel)?;
            let new = NewVersion {
                game: &pending_index.game,
                app_id,
                bytes: &bytes,
                content_hash: &version.content_hash,
                files: version.files,
                mounts: version.mounts.clone(),
                os: &version.os,
                saved_at: version.saved_at,
                pinned: version.pinned,
            };
            self.push_raw_to(&*self.primary, &new, keep, Some(&version))?;
            // Record progress after every version, so a failure part-way
            // never leaves the pending index naming a zip that's gone.
            pending_index.versions.remove(0);
            if pending_index.versions.is_empty() {
                self.pending.delete(&index_rel)?;
            } else {
                self.pending.upload(&index_rel, &serde_json::to_vec_pretty(&pending_index)?)?;
            }
            self.pending.delete(&zip_rel)?;
            flushed.push(version.id);
        }
        if flushed.is_empty() {
            self.pending.delete(&index_rel)?;
        }
        Ok(flushed)
    }

    /// Keep the newest `keep` non-pinned versions; pinned ones always stay.
    /// Removes the rest from `index` and returns their ids (the caller
    /// deletes their zips after saving the index).
    fn apply_retention(index: &mut Index, keep: usize) -> Vec<String> {
        let mut unpinned: Vec<(u64, String)> = index
            .versions
            .iter()
            .filter(|v| !v.pinned)
            .map(|v| (v.created, v.id.clone()))
            .collect();
        if unpinned.len() <= keep {
            return Vec::new();
        }
        unpinned.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        let doomed: Vec<String> = unpinned.split_off(keep).into_iter().map(|(_, id)| id).collect();
        index.versions.retain(|v| !doomed.contains(&v.id));
        doomed
    }

    /// Finds which provider (primary or pending) currently holds a version.
    fn locate(&self, app_id: u64, version_id: &str) -> Result<(&dyn Provider, Index)> {
        // An unreachable primary here just means "not found there" — still
        // check pending; only bail if the version genuinely isn't on either.
        match self.load_index_from(&*self.primary, app_id) {
            Ok(Some(index)) if index.versions.iter().any(|v| v.id == version_id) => {
                return Ok((&*self.primary, index));
            }
            Ok(_) => {}
            Err(err) if crate::provider::is_offline(&err) => {}
            Err(err) => return Err(err),
        }
        if let Some(index) = self.load_index_from(&*self.pending, app_id)? {
            if index.versions.iter().any(|v| v.id == version_id) {
                return Ok((&*self.pending, index));
            }
        }
        anyhow::bail!("version {version_id} not found")
    }

    pub fn set_pinned(&self, app_id: u64, version_id: &str, pinned: bool) -> Result<()> {
        let (provider, index) = self.locate(app_id, version_id)?;
        self.update_index(provider, app_id, &index.game, |index| {
            let version = index
                .versions
                .iter_mut()
                .find(|v| v.id == version_id)
                .with_context(|| format!("version {version_id} not found"))?;
            version.pinned = pinned;
            Ok(())
        })?;
        Ok(())
    }

    /// Manual deletion (D5: pinned conflict versions live until the user
    /// deletes them).
    pub fn remove_version(&self, app_id: u64, version_id: &str) -> Result<()> {
        let (provider, index) = self.locate(app_id, version_id)?;
        self.update_index(provider, app_id, &index.game, |index| {
            index.versions.retain(|v| v.id != version_id);
            Ok(())
        })?;
        provider.delete(&format!("{}/versions/{version_id}.zip", self.game_base(app_id)))
    }

    /// Copies one game's cloud versions from another account's store into
    /// this one — for saves a device uploaded under the wrong Steam account.
    /// Each zip is read back and compared before the index is written, and
    /// versions already here (same id) are skipped. If both sides have an
    /// active head, the older one is pinned (the conflict rule, D5), so
    /// nothing is lost and the newer saves win. Returns how many versions
    /// were (or, with `dry_run`, would be) copied.
    pub fn import_game(&self, source: &Store, app_id: u64, dry_run: bool) -> Result<usize> {
        let Some(src) = source.load_index_from(&*source.primary, app_id)? else {
            return Ok(0);
        };
        let existing = self.load_index_from(&*self.primary, app_id)?.map(|i| i.versions).unwrap_or_default();
        let missing: Vec<&Version> = src.versions.iter().filter(|v| !existing.iter().any(|d| d.id == v.id)).collect();
        if dry_run || missing.is_empty() {
            return Ok(missing.len());
        }
        for version in &missing {
            let from = format!("{}/versions/{}.zip", source.game_base(app_id), version.id);
            let to = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
            let bytes = source.primary.download(&from)?;
            self.primary.upload(&to, &bytes)?;
            if self.primary.download(&to)? != bytes {
                anyhow::bail!("{}: copy of version {} doesn't match the original", src.game, version.id);
            }
        }
        let theirs = Store::head(&src).cloned();
        self.update_index(&*self.primary, app_id, &src.game, |dest| {
            let ours = Store::head(dest).cloned();
            for version in &missing {
                if !dest.versions.iter().any(|d| d.id == version.id) {
                    dest.versions.push((*version).clone());
                }
            }
            if let (Some(ours), Some(theirs)) = (&ours, &theirs) {
                let older = if ours.created <= theirs.created { &ours.id } else { &theirs.id };
                if let Some(v) = dest.versions.iter_mut().find(|v| &v.id == older) {
                    v.pinned = true;
                }
            }
            Ok(())
        })?;
        Ok(missing.len())
    }

    /// Deletes one game's versions and index from this account's cloud.
    /// Only used after `import_game` copied them elsewhere.
    pub fn delete_game(&self, app_id: u64) -> Result<()> {
        let Some(index) = self.load_index_from(&*self.primary, app_id)? else {
            return Ok(());
        };
        for version in &index.versions {
            self.primary.delete(&format!("{}/versions/{}.zip", self.game_base(app_id), version.id))?;
        }
        self.primary.delete(&self.index_rel(app_id))
    }

    /// Removes this account's (now empty) folder from the cloud, best effort.
    pub fn delete_account_folder(&self) {
        let _ = self.primary.delete(&format!("accounts/{}", self.account));
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
            Err(crate::provider::Offline("simulated".into()).into())
        }
        fn download(&self, _rel: &str) -> Result<Vec<u8>> {
            Err(crate::provider::Offline("simulated".into()).into())
        }
        fn upload(&self, _rel: &str, _bytes: &[u8]) -> Result<()> {
            Err(crate::provider::Offline("simulated".into()).into())
        }
        fn delete(&self, _rel: &str) -> Result<()> {
            Err(crate::provider::Offline("simulated".into()).into())
        }
        fn list(&self, _rel: &str) -> Result<Vec<String>> {
            Err(crate::provider::Offline("simulated".into()).into())
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

    fn test_version<'a>(game: &'a str, app_id: u64, bytes: &'a [u8], hash: &'a str, os: &'a str) -> NewVersion<'a> {
        NewVersion { game, app_id, bytes, content_hash: hash, files: 1, mounts: Vec::new(), os, saved_at: None, pinned: false }
    }

    #[test]
    fn machine_names_are_safe_for_version_ids() {
        assert_eq!(sanitize_machine_name("steamdeck\n"), "steamdeck");
        assert_eq!(sanitize_machine_name("Viktors-MacBook-Pro.local"), "Viktors-MacBook-Pro");
        assert_eq!(sanitize_machine_name("my pc/1"), "my-pc-1");
        assert_eq!(sanitize_machine_name("  "), "machine");
        assert_ne!(machine_name(), "machine", "this test machine has a name");
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
            .push_raw(&test_version("Game", 42, b"zip-bytes", "hash1", "mac"), 10)
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
    fn cloud_app_ids_lists_primary_and_pending_games() {
        let primary_root = scratch_dir("primary");
        let pending_root = scratch_dir("pending");
        let online = Store::with_providers(
            Box::new(LocalFolderProvider::new(primary_root.clone())),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        assert!(online.cloud_app_ids().unwrap().is_empty(), "no games folder yet");
        online.push_raw(&test_version("Game", 42, b"zip", "h1", "mac"), 10).unwrap();
        // Another game captured while offline lands in the pending store.
        let offline = Store::with_providers(
            Box::new(AlwaysFailProvider),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        offline.push_raw(&test_version("Other", 7, b"zip", "h2", "mac"), 10).unwrap();
        assert_eq!(online.cloud_app_ids().unwrap(), vec![7, 42]);
        fs::remove_dir_all(&primary_root).ok();
        fs::remove_dir_all(&pending_root).ok();
    }

    #[test]
    fn import_game_merges_accounts_and_pins_the_older_head() {
        let cloud = scratch_dir("cloud");
        let pending = scratch_dir("pending");
        let store = |account: &str| {
            Store::with_providers(
                Box::new(LocalFolderProvider::new(cloud.clone())),
                Box::new(LocalFolderProvider::new(pending.clone())),
                account.to_owned(),
            )
        };
        let (wrong, right) = (store("wrong"), store("right"));
        wrong.push_raw(&test_version("Moved", 1, b"deck-only", "h1", "linux"), 10).unwrap();
        right.push_raw(&test_version("Both", 2, b"mac", "h2", "macos"), 10).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // version ids are per second
        wrong.push_raw(&test_version("Both", 2, b"deck-newer", "h3", "linux"), 10).unwrap();

        assert_eq!(right.import_game(&wrong, 1, true).unwrap(), 1, "dry run counts");
        assert!(right.load_index(1).unwrap().is_none(), "dry run writes nothing");

        assert_eq!(right.import_game(&wrong, 1, false).unwrap(), 1);
        assert_eq!(right.import_game(&wrong, 2, false).unwrap(), 1);
        let moved = right.load_index(1).unwrap().unwrap();
        assert_eq!(right.download_version(1, &moved.versions[0]).unwrap(), b"deck-only");
        let both = right.load_index(2).unwrap().unwrap();
        assert_eq!(both.versions.len(), 2);
        let head = Store::head(&both).unwrap();
        assert_eq!(right.download_version(2, head).unwrap(), b"deck-newer", "newer saves win");
        assert!(both.versions.iter().any(|v| v.pinned && v.os == "macos"), "older head kept, pinned");
        assert_eq!(right.import_game(&wrong, 2, false).unwrap(), 0, "already imported");

        wrong.delete_game(1).unwrap();
        wrong.delete_game(2).unwrap();
        assert!(wrong.load_index(1).unwrap().is_none());
        assert!(wrong.load_index(2).unwrap().is_none());
        fs::remove_dir_all(&cloud).ok();
        fs::remove_dir_all(&pending).ok();
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
                .push_raw(&test_version("Game", 7, b"offline-bytes", "hash-offline", "mac"), 10)
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
            .push_raw(&test_version("Game", 7, b"online-bytes", "hash-online", "mac"), 10)
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

    /// The local folder provider, but uploads fail once `budget` runs out.
    struct Flaky {
        inner: LocalFolderProvider,
        budget: std::sync::Arc<AtomicUsize>,
    }

    impl Provider for Flaky {
        fn exists(&self, rel: &str) -> Result<bool> {
            self.inner.exists(rel)
        }
        fn download(&self, rel: &str) -> Result<Vec<u8>> {
            self.inner.download(rel)
        }
        fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()> {
            if self.budget.try_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(1)).is_err() {
                anyhow::bail!("connection dropped");
            }
            self.inner.upload(rel, bytes)
        }
        fn delete(&self, rel: &str) -> Result<()> {
            self.inner.delete(rel)
        }
        fn list(&self, rel: &str) -> Result<Vec<String>> {
            self.inner.list(rel)
        }
    }

    #[test]
    fn flush_interrupted_part_way_resumes_and_keeps_dates() {
        let primary_root = scratch_dir("flaky-primary");
        let pending_root = scratch_dir("flaky-pending");
        let offline = Store::with_providers(
            Box::new(AlwaysFailProvider),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        let first = offline.push_raw(&test_version("Game", 3, b"one", "h1", "mac"), 10).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = offline.push_raw(&test_version("Game", 3, b"two", "h2", "mac"), 10).unwrap();

        // Enough for the first version (zip + index), not the second.
        let budget = std::sync::Arc::new(AtomicUsize::new(2));
        let flaky = Store::with_providers(
            Box::new(Flaky { inner: LocalFolderProvider::new(primary_root.clone()), budget: budget.clone() }),
            Box::new(LocalFolderProvider::new(pending_root.clone())),
            "acct".to_owned(),
        );
        assert!(flaky.flush_pending(3, 10).is_err());
        let index = flaky.load_index(3).unwrap().unwrap();
        assert_eq!(index.versions.len(), 2, "one moved, one still pending — none lost");
        assert_eq!(index.versions.iter().filter(|v| v.pending).count(), 1);
        assert!(flaky.download_version(3, index.versions.iter().find(|v| v.pending).unwrap()).is_ok());

        budget.store(100, Ordering::SeqCst);
        assert_eq!(flaky.flush_pending(3, 10).unwrap(), vec![second.id.clone()]);
        let index = flaky.load_index(3).unwrap().unwrap();
        assert!(index.versions.iter().all(|v| !v.pending));
        for original in [&first, &second] {
            let moved = index.versions.iter().find(|v| v.id == original.id).expect("same id");
            assert_eq!(moved.created, original.created, "keeps its original date");
        }
        assert!(flaky.flush_pending(3, 10).unwrap().is_empty());
        fs::remove_dir_all(&primary_root).ok();
        fs::remove_dir_all(&pending_root).ok();
    }

    /// The local folder provider; just before its first conditional index
    /// write, `meanwhile` runs (another machine writing the same index).
    struct Racy {
        inner: LocalFolderProvider,
        meanwhile: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Provider for Racy {
        fn exists(&self, rel: &str) -> Result<bool> {
            self.inner.exists(rel)
        }
        fn download(&self, rel: &str) -> Result<Vec<u8>> {
            self.inner.download(rel)
        }
        fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()> {
            self.inner.upload(rel, bytes)
        }
        fn delete(&self, rel: &str) -> Result<()> {
            self.inner.delete(rel)
        }
        fn list(&self, rel: &str) -> Result<Vec<String>> {
            self.inner.list(rel)
        }
        fn upload_if(&self, rel: &str, bytes: &[u8], expected: Option<&str>) -> Result<bool> {
            if let Some(other_machine) = self.meanwhile.lock().unwrap().take() {
                other_machine();
            }
            self.inner.upload_if(rel, bytes, expected)
        }
    }

    #[test]
    fn concurrent_index_writes_keep_both_versions() {
        let cloud = scratch_dir("racy-cloud");
        let pending = scratch_dir("racy-pending");
        let plain = |dir: &PathBuf| Box::new(LocalFolderProvider::new(dir.clone()));
        let base = Store::with_providers(plain(&cloud), plain(&pending), "acct".into());
        base.push_raw(&test_version("Game", 5, b"base", "h0", "mac"), 10).unwrap();

        let other = Store::with_providers(plain(&cloud), plain(&scratch_dir("racy-pending-2")), "acct".into());
        let racy = Store::with_providers(
            Box::new(Racy {
                inner: LocalFolderProvider::new(cloud.clone()),
                meanwhile: std::sync::Mutex::new(Some(Box::new(move || {
                    other.push_raw(&test_version("Game", 5, b"other", "h-other", "linux"), 10).unwrap();
                }))),
            }),
            plain(&pending),
            "acct".into(),
        );
        std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct ids
        racy.push_raw(&test_version("Game", 5, b"ours", "h-ours", "mac"), 10).unwrap();

        let hashes: Vec<String> = base.load_index(5).unwrap().unwrap().versions.into_iter().map(|v| v.content_hash).collect();
        for h in ["h0", "h-other", "h-ours"] {
            assert!(hashes.iter().any(|x| x == h), "{h} missing from {hashes:?}");
        }
        fs::remove_dir_all(&cloud).ok();
        fs::remove_dir_all(&pending).ok();
    }

    #[test]
    fn a_damaged_cloud_index_is_an_error_not_an_empty_cloud() {
        let cloud = scratch_dir("damaged");
        let store = Store::with_providers(
            Box::new(LocalFolderProvider::new(cloud.clone())),
            Box::new(LocalFolderProvider::new(scratch_dir("damaged-pending"))),
            "acct".into(),
        );
        LocalFolderProvider::new(cloud.clone()).upload("accounts/acct/games/9/index.json", b"{ not json").unwrap();
        assert!(store.load_index(9).is_err());
        fs::remove_dir_all(&cloud).ok();
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
            .push_raw(&test_version("Game", 1, b"data", "hash", "mac"), 10)
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
