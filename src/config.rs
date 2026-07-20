//! User configuration (per-game overrides, D15/D9) and per-machine sync
//! state. Both live in the config dir; state is what lets sync distinguish
//! "changed here" from "changed there" (three-state comparison).
//! Note: DESIGN.md planned rusqlite for state; JSON suffices until
//! file-level tracking arrives with the daemon.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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

fn load_json<T: Default + for<'de> Deserialize<'de>>(name: &str) -> T {
    config_dir()
        .ok()
        .and_then(|dir| fs::read(dir.join(name)).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = config_dir()?.join(name);
    fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", path.display()))
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

    pub fn set(&mut self, account: &str, app_id: u64, version: &str, hash: &str) {
        self.games.insert(
            format!("{account}/{app_id}"),
            GameState {
                last_version: version.to_owned(),
                last_hash: hash.to_owned(),
                synced_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
        );
    }
}
