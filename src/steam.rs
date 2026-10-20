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
        // Where Steam itself says it lives (covers installs on other drives).
        if let Some(path) = windows_steam_path() {
            v.push(path);
        }
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

#[cfg(windows)]
fn windows_steam_path() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam").ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path))
}

#[cfg(not(windows))]
fn windows_steam_path() -> Option<PathBuf> {
    None
}

/// All Steam library directories (each containing a `steamapps` folder).
/// A path spelling-independent key: on Windows the registry says
/// `c:/program files (x86)/steam` while libraryfolders.vdf says
/// `C:\\Program Files (x86)\\Steam` for the same folder.
fn normalize_windows_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn library_key(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    if cfg!(windows) {
        // canonicalize adds a `\\?\` prefix on Windows.
        normalize_windows_path(text.trim_start_matches(r"\\?\"))
    } else {
        text.into_owned()
    }
}

pub fn libraries(root: &Path) -> Result<Vec<PathBuf>> {
    let mut libs = vec![root.to_path_buf()];
    let mut seen = vec![library_key(root)];
    let lf = root.join("steamapps/libraryfolders.vdf");
    if let Ok(text) = fs::read_to_string(&lf) {
        let doc = vdf::parse(&text).context("parsing libraryfolders.vdf")?;
        if let Some(folders) = vdf::get_obj(&doc, "libraryfolders") {
            for value in folders.values() {
                if let Some(entry) = value.as_obj() {
                    if let Some(path) = vdf::get_str(entry, "path") {
                        let path = PathBuf::from(path);
                        let key = library_key(&path);
                        if path.join("steamapps").is_dir() && !seen.contains(&key) {
                            seen.push(key);
                            libs.push(path);
                        }
                    }
                }
            }
        }
    }
    Ok(libs)
}

/// Steam appids for shared prerequisite/runtime installs that show up as
/// ordinary `appmanifest_*.acf` entries alongside real games but aren't
/// games themselves, so they'd otherwise appear as permanently
/// "not in manifest" rows. Steam gives no reliable type flag in the ACF
/// itself (that lives in the binary appinfo.vdf), so this is a denylist of
/// known IDs. Compatibility tools (Proton, Steam Linux Runtime) don't need
/// listing: `installed_games` recognizes them by their toolmanifest.vdf.
const NON_GAME_APP_IDS: &[u64] = &[
    228980, // Steamworks Common Redistributables
];

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
            if NON_GAME_APP_IDS.contains(&app_id) || games.iter().any(|g: &InstalledGame| g.app_id == app_id) {
                continue;
            }
            let install_dir = vdf::get_str(state, "installdir").unwrap_or_default().to_owned();
            // Compatibility tools (Proton, Steam Linux Runtime, ...) all ship
            // a toolmanifest.vdf in their install dir; games never do.
            if !install_dir.is_empty() && steamapps.join("common").join(&install_dir).join("toolmanifest.vdf").is_file() {
                continue;
            }
            games.push(InstalledGame {
                app_id,
                name: vdf::get_str(state, "name").unwrap_or("?").to_owned(),
                install_dir,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_acf(steamapps: &Path, app_id: u64, name: &str, install_dir: &str) {
        let acf = format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{install_dir}\"\n}}\n"
        );
        fs::write(steamapps.join(format!("appmanifest_{app_id}.acf")), acf).unwrap();
        fs::create_dir_all(steamapps.join("common").join(install_dir)).unwrap();
    }

    #[test]
    fn windows_path_spellings_of_one_folder_compare_equal() {
        assert_eq!(
            normalize_windows_path("c:/program files (x86)/steam"),
            normalize_windows_path(r"C:\Program Files (x86)\Steam\")
        );
        assert_ne!(normalize_windows_path("D:/SteamLibrary"), normalize_windows_path("C:/SteamLibrary"));
    }

    #[test]
    fn a_library_listed_twice_yields_each_game_once() {
        let library = std::env::temp_dir().join(format!("yasgm-steam-dup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&library);
        let steamapps = library.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        write_acf(&steamapps, 588650, "Dead Cells", "Dead Cells");
        // The same folder spelled two ways, as on Windows.
        let again = library.join("steamapps").join("..");
        let games = installed_games(&[library.clone(), again.clone()]).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(library_key(&library), library_key(&again));
        // And libraryfolders.vdf pointing back at the root doesn't add it.
        fs::write(
            steamapps.join("libraryfolders.vdf"),
            format!("\"libraryfolders\"\n{{\n \"0\"\n {{\n \"path\" \"{}\"\n }}\n}}\n", again.display()),
        )
        .unwrap();
        assert_eq!(libraries(&library).unwrap().len(), 1);
        fs::remove_dir_all(&library).unwrap();
    }

    #[test]
    fn skips_compatibility_tools_and_redistributables() {
        let library = std::env::temp_dir().join(format!("yasgm-steam-lib-{}", std::process::id()));
        let _ = fs::remove_dir_all(&library);
        let steamapps = library.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        write_acf(&steamapps, 2140020, "Stronghold: Definitive Edition", "Stronghold Definitive Edition");
        write_acf(&steamapps, 4628740, "Proton 11.0 (ARM64)", "Proton 11.0 (ARM64)");
        fs::write(steamapps.join("common/Proton 11.0 (ARM64)/toolmanifest.vdf"), "\"manifest\" {}").unwrap();
        write_acf(&steamapps, 228980, "Steamworks Common Redistributables", "Steamworks Shared");

        let games = installed_games(&[library.clone()]).unwrap();
        let ids: Vec<u64> = games.iter().map(|g| g.app_id).collect();
        assert_eq!(ids, vec![2140020]);
        fs::remove_dir_all(&library).unwrap();
    }
}
