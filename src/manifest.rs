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

/// D7: a local, user-maintained manifest in the same schema, for games
/// missing from the community manifest or entries that need extra/corrected
/// paths — without waiting on a PCGamingWiki edit to land upstream. Absent
/// by default; not an error if it doesn't exist.
/// The files `load` reads: the cached community manifest and the local
/// overrides (either may not exist).
pub fn source_paths() -> Vec<PathBuf> {
    [cache_dir().map(|d| d.join("manifest.yaml")), local_manifest_path()].into_iter().flatten().collect()
}

pub fn local_manifest_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no config directory on this platform")?
        .join("yasgm");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("ludusavi.yaml"))
}

fn load_local_overrides() -> Result<Manifest> {
    let path = local_manifest_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Merge local overrides into the downloaded manifest. An entry with no
/// `files` is ignored entirely — that's a placeholder (`files: {}`) from
/// the generated skeleton (`manifest-gaps --skeleton`) until someone fills
/// it in. A filled entry *replaces* the community entry's save rules, so a
/// wrong upstream path can be removed, not just supplemented;
/// `steam`/`cloud` are replaced only when the override sets them. A new
/// name is inserted and joins its AppID pool normally via `steam_index`
/// (other community entries sharing that AppID are left as they are).
fn merge_manifest(base: &mut Manifest, overrides: Manifest) {
    for (name, over) in overrides {
        if over.files.is_empty() {
            continue;
        }
        match base.get_mut(&name) {
            Some(existing) => {
                existing.files = over.files;
                if over.steam.is_some() {
                    existing.steam = over.steam;
                }
                if over.cloud.is_some() {
                    existing.cloud = over.cloud;
                }
            }
            None => {
                base.insert(name, over);
            }
        }
    }
}

/// Fetch the manifest, using the cached copy when the ETag still matches or
/// the network is unavailable, then merge in local overrides (D7) if any.
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
            // yaml before etag: a crash in between leaves a fresh yaml with
            // the old etag, which only costs one redundant download.
            crate::fsutil::write_atomic(&yaml_path, &body)?;
            if let Some(etag) = etag {
                crate::fsutil::write_atomic(&etag_path, etag.as_bytes())?;
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
    let mut manifest: Manifest = serde_yaml::from_str(&text).context("parsing manifest")?;

    let overrides = load_local_overrides()?;
    if !overrides.is_empty() {
        eprintln!(
            "manifest: {} local override(s) from {}",
            overrides.len(),
            local_manifest_path()?.display()
        );
        merge_manifest(&mut manifest, overrides);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> FileRule {
        FileRule { tags: vec!["save".to_owned()], when: Vec::new() }
    }

    fn game(files: &[&str]) -> Game {
        Game {
            files: files.iter().map(|f| (f.to_string(), rule())).collect(),
            steam: None,
            cloud: None,
        }
    }

    #[test]
    fn new_name_is_inserted() {
        let mut base: Manifest = [("Known Game".to_owned(), game(&["<home>/known"]))].into();
        let overrides: Manifest = [("New Game".to_owned(), game(&["<home>/new"]))].into();
        merge_manifest(&mut base, overrides);
        assert_eq!(base.len(), 2);
        assert!(base.contains_key("New Game"));
    }

    #[test]
    fn filled_entry_replaces_the_community_rules() {
        let mut base: Manifest = [("Known Game".to_owned(), game(&["<home>/original"]))].into();
        let overrides: Manifest = [("Known Game".to_owned(), game(&["<home>/fixed"]))].into();
        merge_manifest(&mut base, overrides);
        assert_eq!(base.len(), 1, "must not create a duplicate entry");
        let files = &base["Known Game"].files;
        assert!(files.contains_key("<home>/fixed"));
        assert!(!files.contains_key("<home>/original"), "a filled override replaces upstream rules");
    }

    #[test]
    fn unfilled_skeleton_entries_are_ignored() {
        let yaml = "\"Known Game\":\n  files: {}\n\"New Game\":\n  files: {}\n";
        let overrides: Manifest = serde_yaml::from_str(yaml).unwrap();
        let mut base: Manifest = [("Known Game".to_owned(), game(&["<home>/original"]))].into();
        merge_manifest(&mut base, overrides);
        assert_eq!(base.len(), 1, "an empty new entry must not be inserted");
        assert!(base["Known Game"].files.contains_key("<home>/original"), "an empty entry must not wipe rules");
    }

    #[test]
    fn override_steam_id_replaces_missing_metadata() {
        let mut base: Manifest = [("Known Game".to_owned(), game(&["<home>/original"]))].into();
        let mut with_id = game(&["<home>/original"]);
        with_id.steam = Some(SteamInfo { id: Some(42) });
        let overrides: Manifest = [("Known Game".to_owned(), with_id)].into();
        merge_manifest(&mut base, overrides);
        assert_eq!(base["Known Game"].steam.as_ref().and_then(|s| s.id), Some(42));
    }
}
