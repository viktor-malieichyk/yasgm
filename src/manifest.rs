//! Ludusavi Manifest ingestion: download with ETag caching, parse, and index
//! by Steam AppID. Data derives from PCGamingWiki (CC BY-SA/NC attribution
//! lives in the wiki); the manifest repo itself is MIT.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml";

pub type Manifest = HashMap<String, Game>;

#[derive(Debug, Deserialize)]
pub struct Game {
    #[serde(default)]
    pub files: HashMap<String, FileRule>,
    #[serde(default)]
    pub steam: Option<SteamInfo>,
    #[serde(default)]
    pub cloud: Option<Cloud>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileRule {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub when: Vec<Constraint>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Constraint {
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub store: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SteamInfo {
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Cloud {
    #[serde(default)]
    pub steam: Option<bool>,
}

impl FileRule {
    /// D1: saves only. Untagged rules are included conservatively.
    pub fn is_save(&self) -> bool {
        self.tags.is_empty() || self.tags.iter().any(|t| t == "save")
    }
}

fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("no cache directory on this platform")?
        .join("yasgm");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Fetch the manifest, using the cached copy when the ETag still matches or
/// the network is unavailable.
pub fn load() -> Result<Manifest> {
    let cache = cache_dir()?;
    let yaml_path = cache.join("manifest.yaml");
    let etag_path = cache.join("manifest.etag");

    let cached_etag = fs::read_to_string(&etag_path).ok();
    let mut request = ureq::get(MANIFEST_URL);
    if yaml_path.exists() {
        if let Some(etag) = &cached_etag {
            request = request.set("If-None-Match", etag.trim());
        }
    }

    match request.call() {
        Ok(response) if response.status() == 304 => {
            eprintln!("manifest: cache up to date");
        }
        Ok(response) => {
            let etag = response.header("etag").map(str::to_owned);
            let mut body = Vec::new();
            response
                .into_reader()
                .read_to_end(&mut body)
                .context("downloading manifest")?;
            fs::write(&yaml_path, &body)?;
            if let Some(etag) = etag {
                fs::write(&etag_path, etag)?;
            }
            eprintln!(
                "manifest: downloaded {:.1} MB",
                body.len() as f64 / 1_048_576.0
            );
        }
        Err(err) if yaml_path.exists() => {
            eprintln!("manifest: offline ({err}); using cached copy");
        }
        Err(err) => return Err(err).context("downloading manifest (no cache available)"),
    }

    let text = fs::read_to_string(&yaml_path)?;
    let manifest: Manifest = serde_yaml::from_str(&text).context("parsing manifest")?;
    Ok(manifest)
}

/// Index manifest entries by Steam AppID. One AppID can map to several
/// entries (e.g. "Divinity: Original Sin II" and its Definitive Edition both
/// carry 435150); names are sorted for deterministic merging.
pub fn steam_index(manifest: &Manifest) -> HashMap<u64, Vec<String>> {
    let mut index: HashMap<u64, Vec<String>> = HashMap::new();
    for (name, game) in manifest {
        if let Some(id) = game.steam.as_ref().and_then(|s| s.id) {
            index.entry(id).or_default().push(name.clone());
        }
    }
    for names in index.values_mut() {
        names.sort();
    }
    index
}
