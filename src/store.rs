//! Cloud snapshot store on top of the OneDrive app folder.
//!
//! Layout (per D4, per Steam account):
//!   accounts/<steam-account-id>/games/<appid>/index.json
//!   accounts/<steam-account-id>/games/<appid>/versions/<version-id>.zip
//!
//! Versions are immutable. The **active head** is the newest *non-pinned*
//! version; pinned versions are preserved archives (conflict losers and
//! pre-restore states, D5/D14) that never compete for "latest" and are never
//! pruned automatically (retention keeps the newest N non-pinned, D9).

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
}

pub struct Store {
    provider: Box<dyn Provider>,
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

impl Store {
    pub fn new(provider: Box<dyn Provider>, account: String) -> Store {
        Store { provider, account }
    }

    fn game_base(&self, app_id: u64) -> String {
        format!("accounts/{}/games/{}", self.account, app_id)
    }

    pub fn load_index(&self, app_id: u64) -> Result<Option<Index>> {
        let rel = format!("{}/index.json", self.game_base(app_id));
        if !self.provider.exists(&rel)? {
            return Ok(None);
        }
        let bytes = self.provider.download(&rel)?;
        Ok(Some(serde_json::from_slice(&bytes).context("parsing index.json")?))
    }

    fn save_index(&self, index: &Index) -> Result<()> {
        let rel = format!("{}/index.json", self.game_base(index.app_id));
        self.provider.upload(&rel, &serde_json::to_vec_pretty(index)?)
    }

    /// Active head: newest non-pinned version.
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
    /// "make this content the active head again" step).
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
        let created = unix_now();
        let mut id = format!("{}_{}", timestamp_id(created), machine_name());

        let mut index = self.load_index(app_id)?.unwrap_or(Index {
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
        };

        let zip_rel = format!("{}/versions/{id}.zip", self.game_base(app_id));
        self.provider.upload(&zip_rel, zip_bytes)?;
        index.versions.push(version.clone());
        self.apply_retention(&mut index, keep)?;
        self.save_index(&index)?;

        let total: u64 = index.versions.iter().map(|v| v.size).sum();
        if total > SIZE_WARN_BYTES {
            eprintln!(
                "warning: {game_name} now stores {:.1} GB in the cloud (D9 threshold is 1 GB); \
                 consider lowering its version count (`yasgm config {app_id} --keep N`)",
                total as f64 / 1_073_741_824.0
            );
        }
        Ok(version)
    }

    /// Keep the newest `keep` non-pinned versions; pinned ones always stay.
    fn apply_retention(&self, index: &mut Index, keep: usize) -> Result<()> {
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
            self.provider.delete(&zip_rel)?;
        }
        index.versions.retain(|v| !doomed.contains(&v.id));
        Ok(())
    }

    pub fn set_pinned(&self, app_id: u64, version_id: &str, pinned: bool) -> Result<()> {
        let mut index = self
            .load_index(app_id)?
            .context("no cloud versions for this game")?;
        let version = index
            .versions
            .iter_mut()
            .find(|v| v.id == version_id)
            .with_context(|| format!("version {version_id} not found"))?;
        version.pinned = pinned;
        self.save_index(&index)
    }

    /// Manual deletion (D5: pinned conflict versions live until the user
    /// deletes them).
    pub fn remove_version(&self, app_id: u64, version_id: &str) -> Result<()> {
        let mut index = self
            .load_index(app_id)?
            .context("no cloud versions for this game")?;
        if !index.versions.iter().any(|v| v.id == version_id) {
            anyhow::bail!("version {version_id} not found");
        }
        let zip_rel = format!("{}/versions/{version_id}.zip", self.game_base(app_id));
        self.provider.delete(&zip_rel)?;
        index.versions.retain(|v| v.id != version_id);
        self.save_index(&index)
    }

    pub fn download_version(&self, app_id: u64, version: &Version) -> Result<Vec<u8>> {
        let zip_rel = format!("{}/versions/{}.zip", self.game_base(app_id), version.id);
        self.provider.download(&zip_rel)
    }
}
