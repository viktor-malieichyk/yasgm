//! User configuration (per-game overrides, D15/D9) and per-machine sync
//! state. Both live in the config dir; state is what lets sync distinguish
//! "changed here" from "changed there" (three-state comparison).
//! Note: DESIGN.md planned rusqlite for state; JSON suffices until
//! file-level tracking arrives with the daemon.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModeOverride {
    #[default]
    Auto,
    Sync,
    Backup,
    Off,
}

impl ModeOverride {
    pub fn parse(s: &str) -> Option<ModeOverride> {
        match s {
            "auto" => Some(ModeOverride::Auto),
            "sync" => Some(ModeOverride::Sync),
            "backup" => Some(ModeOverride::Backup),
            "off" => Some(ModeOverride::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ModeOverride::Auto => "auto",
            ModeOverride::Sync => "sync",
            ModeOverride::Backup => "backup",
            ModeOverride::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameConfig {
    #[serde(default)]
    pub mode: ModeOverride,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<usize>,
}

/// Which cloud backend `Store` talks to (D8). Global, not per-game — the
/// whole cloud layout (`accounts/<id>/games/<appid>/...`) lives under
/// whichever provider is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    Onedrive,
    Local { path: PathBuf },
    /// Disconnected on purpose (`yasgm provider disconnect`): nothing syncs
    /// until a provider is connected again. Not the default — an existing
    /// install without a provider setting still means OneDrive.
    None,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig::Onedrive
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub games: HashMap<u64, GameConfig>,
    #[serde(default)]
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub last_version: String,
    pub last_hash: String,
    /// Local files' hash right after that sync, when it differs from
    /// `last_hash` (see `sync::LastSync::local_hash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_hash: Option<String>,
    pub synced_at: u64,
}

/// Keyed by "<account>/<appid>".
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub games: HashMap<String, GameState>,
}

fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no config directory on this platform")?
        .join("yasgm");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Where `Config` lives.
pub fn config_path() -> Option<PathBuf> {
    config_dir().ok().map(|d| d.join("config.json"))
}

fn load_json<T: Default + for<'de> Deserialize<'de>>(name: &str) -> T {
    match config_dir() {
        Ok(dir) => load_json_from(&dir.join(name)),
        Err(_) => T::default(),
    }
}

/// A missing file means defaults. A damaged one (writes are atomic, so
/// that's disk trouble or a hand edit) is moved aside — not silently read
/// as empty and then overwritten on the next save — and reported.
fn load_json_from<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> T {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(err) => {
            eprintln!("warning: can't read {} ({err}); using defaults", path.display());
            return T::default();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".damaged-{secs}"));
            let moved = fs::rename(path, &aside).is_ok();
            eprintln!(
                "warning: {} is damaged ({err}); {}using defaults",
                path.display(),
                if moved { format!("moved it to {} and ", Path::new(&aside).display()) } else { String::new() }
            );
            T::default()
        }
    }
}

fn save_json<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = config_dir()?.join(name);
    crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(value)?)
}

impl Config {
    pub fn load() -> Config {
        load_json("config.json")
    }

    pub fn save(&self) -> Result<()> {
        save_json("config.json", self)
    }

    pub fn game(&self, app_id: u64) -> GameConfig {
        self.games.get(&app_id).cloned().unwrap_or_default()
    }
}

impl State {
    pub fn load() -> State {
        load_json("state.json")
    }

    pub fn save(&self) -> Result<()> {
        save_json("state.json", self)
    }

    pub fn get(&self, account: &str, app_id: u64) -> Option<&GameState> {
        self.games.get(&format!("{account}/{app_id}"))
    }

    pub fn set(&mut self, account: &str, app_id: u64, version: &str, hash: &str, local_hash: Option<&str>) {
        self.games.insert(
            format!("{account}/{app_id}"),
            GameState {
                last_version: version.to_owned(),
                last_hash: hash.to_owned(),
                local_hash: local_hash.map(str::to_owned),
                synced_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_round_trips_including_disconnected() {
        for json in [r#"{"type":"onedrive"}"#, r#"{"type":"local","path":"/x"}"#, r#"{"type":"none"}"#] {
            let provider: ProviderConfig = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&provider).unwrap(), json);
        }
    }

    #[test]
    fn a_damaged_file_is_moved_aside_and_reported() {
        let dir = std::env::temp_dir().join(format!("yasgm-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        assert!(load_json_from::<State>(&path).games.is_empty(), "missing → defaults");
        fs::write(&path, b"{\"games\": {tru").unwrap();
        assert!(load_json_from::<State>(&path).games.is_empty());
        assert!(!path.exists(), "damaged file moved away");
        let aside: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(aside.iter().any(|n| n.starts_with("state.json.damaged-")), "{aside:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_config_without_a_provider_still_means_onedrive() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(matches!(cfg.provider, ProviderConfig::Onedrive));
    }
}
