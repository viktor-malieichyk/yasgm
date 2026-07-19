//! Steam library discovery: locate the Steam root, enumerate libraries from
//! libraryfolders.vdf, read appmanifest_*.acf, and detect Steam Cloud usage.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::vdf;

#[derive(Debug)]
pub struct InstalledGame {
    pub app_id: u64,
    pub name: String,
    pub install_dir: String,
    pub library: PathBuf,
}

pub fn find_steam_root() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![home.join("Library/Application Support/Steam")]
    } else if cfg!(target_os = "windows") {
        let mut v = Vec::new();
        if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
            v.push(PathBuf::from(pf).join("Steam"));
        }
        v.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        v
    } else {
        vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            // Flatpak Steam on SteamOS-like setups
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ]
    };
    candidates
        .into_iter()
        .find(|p| p.join("steamapps").is_dir())
}

/// All Steam library directories (each containing a `steamapps` folder).
pub fn libraries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut libs = vec![root.to_path_buf()];
    let lf = root.join("steamapps/libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&lf) {
        let doc = vdf::parse(&text).context("parsing libraryfolders.vdf")?;
        if let Some(folders) = vdf::get_obj(&doc, "libraryfolders") {
            for value in folders.values() {
                if let Some(entry) = value.as_obj() {
                    if let Some(path) = vdf::get_str(entry, "path") {
                        let path = PathBuf::from(path);
                        if path.join("steamapps").is_dir() && !libs.contains(&path) {
                            libs.push(path);
                        }
                    }
                }
            }
        }
    }
    Ok(libs)
}

pub fn installed_games(libraries: &[PathBuf]) -> Result<Vec<InstalledGame>> {
    let mut games = Vec::new();
    for library in libraries {
        let steamapps = library.join("steamapps");
        let entries = match fs::read_dir(&steamapps) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if !file_name.starts_with("appmanifest_") || !file_name.ends_with(".acf") {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let doc = match vdf::parse(&text) {
                Ok(doc) => doc,
                Err(err) => {
                    eprintln!("warning: failed to parse {}: {err}", path.display());
                    continue;
                }
            };
            let Some(state) = vdf::get_obj(&doc, "appstate") else {
                continue;
            };
            let Some(app_id) = vdf::get_str(state, "appid").and_then(|s| s.parse().ok()) else {
                continue;
            };
            games.push(InstalledGame {
                app_id,
                name: vdf::get_str(state, "name").unwrap_or("?").to_owned(),
                install_dir: vdf::get_str(state, "installdir").unwrap_or_default().to_owned(),
                library: library.clone(),
            });
        }
    }
    games.sort_by_key(|g| g.name.to_lowercase());
    Ok(games)
}

/// Heuristic: Steam Cloud is active for an app if any account's userdata has a
/// remotecache.vdf (or remote dir) for it. Complements the manifest's
/// `cloud.steam` flag; appinfo.vdf (`ufs` section) parsing is a Phase 1 TODO.
pub fn steam_cloud_active(root: &Path, app_id: u64) -> bool {
    let userdata = root.join("userdata");
    let Ok(accounts) = fs::read_dir(&userdata) else {
        return false;
    };
    for account in accounts.flatten() {
        let app_dir = account.path().join(app_id.to_string());
        if app_dir.join("remotecache.vdf").is_file() || app_dir.join("remote").is_dir() {
            return true;
        }
    }
    false
}

/// Steam account IDs (accountid3) present on this machine — the basis for the
/// per-account cloud namespace (D4).
pub fn account_ids(root: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join("userdata")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.chars().all(|c| c.is_ascii_digit()) && name != "0" {
                ids.push(name);
            }
        }
    }
    ids.sort();
    ids
}
