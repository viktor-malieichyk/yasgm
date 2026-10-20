//! Game icons: the 32×32 "community icon" Steam shows in its library list.
//! Works the same on every platform: Steam's own image cache under
//! `<Steam root>/appcache/librarycache` has one layout everywhere, so it is
//! checked first (offline, free). Icons Steam hasn't cached are fetched once
//! via Steam's public store API (icon hash, no key needed) + CDN into our
//! cache dir.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use base64::Engine;

const STORE_API: &str = "https://api.steampowered.com/IStoreBrowseService/GetItems/v1/";
const ICON_CDN: &str = "https://cdn.cloudflare.steamstatic.com/steamcommunity/public/images/apps";
/// Store-API lookups per request.
const BATCH: usize = 50;
/// Apps with no icon (delisted, tools, failed download) aren't re-queried
/// on every GUI start, only after this long.
const MISS_RETRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("no cache directory on this platform")?
        .join("yasgm/icons");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The SHA-1 Steam names icon files by (40 hex chars). Also gates the hash
/// from the store API before it goes into a URL.
fn is_icon_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Steam's cached icon for `app_id`. Current layout (Steam 2024+):
/// `librarycache/<appid>/<icon sha1>.jpg`; older clients:
/// `librarycache/<appid>_icon.jpg`.
pub fn steam_cached(steam_root: &Path, app_id: u64) -> Option<PathBuf> {
    let cache = steam_root.join("appcache/librarycache");
    if let Ok(entries) = fs::read_dir(cache.join(app_id.to_string())) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().and_then(|n| n.strip_suffix(".jpg")).is_some_and(is_icon_hash) {
                return Some(entry.path());
            }
        }
    }
    let legacy = cache.join(format!("{app_id}_icon.jpg"));
    legacy.is_file().then_some(legacy)
}

/// Best local icon: Steam's cache, else one we downloaded earlier.
pub fn resolve(steam_root: Option<&Path>, dir: &Path, app_id: u64) -> Option<PathBuf> {
    steam_root.and_then(|root| steam_cached(root, app_id)).or_else(|| {
        let ours = dir.join(format!("{app_id}.jpg"));
        ours.is_file().then_some(ours)
    })
}

fn recent_miss(dir: &Path, app_id: u64) -> bool {
    fs::metadata(dir.join(format!("{app_id}.miss")))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MISS_RETRY)
}

fn mark_miss(dir: &Path, app_id: u64) {
    let _ = fs::write(dir.join(format!("{app_id}.miss")), b"");
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build()
}

/// appid → community-icon hash, for the apps the store knows.
fn lookup_hashes(agent: &ureq::Agent, app_ids: &[u64]) -> Result<HashMap<u64, String>> {
    let ids: Vec<_> = app_ids.iter().map(|id| serde_json::json!({ "appid": id })).collect();
    let input = serde_json::json!({
        "ids": ids,
        "context": { "language": "english", "country_code": "US" },
        "data_request": { "include_assets": true },
    });
    let body = agent
        .get(STORE_API)
        .query("input_json", &input.to_string())
        .call()
        .context("querying Steam store for icons")?
        .into_string()?;
    let doc: serde_json::Value = serde_json::from_str(&body).context("parsing Steam store response")?;
    let mut hashes = HashMap::new();
    for item in doc["response"]["store_items"].as_array().into_iter().flatten() {
        if let (Some(id), Some(hash)) = (item["appid"].as_u64(), item["assets"]["community_icon"].as_str()) {
            if is_icon_hash(hash) {
                hashes.insert(id, hash.to_owned());
            }
        }
    }
    Ok(hashes)
}

/// Downloads icons for `app_ids` into `dir`, skipping apps that recently
/// had none. Per-app failures are recorded as misses; a failed store lookup
/// (e.g. offline) is returned as an error without recording misses, so the
/// next online run tries again.
pub fn fetch(dir: &Path, app_ids: &[u64]) -> Result<usize> {
    let due: Vec<u64> = app_ids.iter().copied().filter(|&id| !recent_miss(dir, id)).collect();
    let agent = agent();
    let mut fetched = 0;
    for chunk in due.chunks(BATCH) {
        let hashes = lookup_hashes(&agent, chunk)?;
        for &app_id in chunk {
            let Some(hash) = hashes.get(&app_id) else {
                mark_miss(dir, app_id);
                continue;
            };
            match download(&agent, app_id, hash) {
                Ok(bytes) => {
                    crate::fsutil::write_atomic(&dir.join(format!("{app_id}.jpg")), &bytes)?;
                    fetched += 1;
                }
                Err(err) => {
                    eprintln!("icon for {app_id}: {err:#}");
                    mark_miss(dir, app_id);
                }
            }
        }
    }
    Ok(fetched)
}

fn download(agent: &ureq::Agent, app_id: u64, hash: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    agent
        .get(&format!("{ICON_CDN}/{app_id}/{hash}.jpg"))
        .call()?
        .into_reader()
        .take(1024 * 1024)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    Ok(bytes)
}

/// `data:` URL for an icon file, so a webview can show it without file access.
pub fn data_url(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mime = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) { "image/png" } else { "image/jpeg" };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yasgm-icons-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_icon_in_current_and_legacy_steam_layouts() {
        let root = temp_dir("layouts");
        let cache = root.join("appcache/librarycache");
        fs::create_dir_all(cache.join("10")).unwrap();
        fs::write(cache.join("10/header.jpg"), b"x").unwrap();
        fs::write(cache.join("10/6b0312cda02f5f777efa2f3318c307ff9acafbb5.jpg"), b"x").unwrap();
        fs::write(cache.join("20_icon.jpg"), b"x").unwrap();

        assert_eq!(
            steam_cached(&root, 10).unwrap().file_name().unwrap(),
            "6b0312cda02f5f777efa2f3318c307ff9acafbb5.jpg"
        );
        assert_eq!(steam_cached(&root, 20).unwrap().file_name().unwrap(), "20_icon.jpg");
        assert!(steam_cached(&root, 30).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn falls_back_to_downloaded_icon_and_skips_recent_misses() {
        let dir = temp_dir("fallback");
        fs::write(dir.join("40.jpg"), b"x").unwrap();
        assert_eq!(resolve(None, &dir, 40), Some(dir.join("40.jpg")));
        assert!(resolve(None, &dir, 50).is_none());

        assert!(!recent_miss(&dir, 50));
        mark_miss(&dir, 50);
        assert!(recent_miss(&dir, 50));
        fs::remove_dir_all(&dir).unwrap();
    }
}
