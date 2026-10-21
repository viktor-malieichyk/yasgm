//! `Ctx`: what the game-facing commands work from (manifest, Steam library,
//! signed-in account), and building the cloud `Store`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};

use crate::config::{self, ModeOverride};
use crate::resolve::Os;
use crate::store::{self, Store};
use crate::{local, manifest, onedrive, steam};

/// Everything the game-facing commands need, loaded once.
pub(crate) struct Ctx {
    pub(crate) manifest: manifest::Manifest,
    pub(crate) by_app_id: HashMap<u64, Vec<String>>,
    pub(crate) root: PathBuf,
    pub(crate) libraries: Vec<PathBuf>,
    pub(crate) games: Vec<steam::InstalledGame>,
    pub(crate) account: String,
    pub(crate) os: Os,
}

/// The union of all manifest entries sharing one Steam AppID (some games have
/// several, e.g. original + Definitive Edition under the same ID).
pub(crate) struct MergedGame {
    pub(crate) files: HashMap<String, manifest::FileRule>,
    pub(crate) cloud_steam: bool,
}

pub(crate) fn load_ctx() -> Result<Ctx> {
    let manifest = manifest::load()?;
    let by_app_id = manifest::steam_index(&manifest);
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let libraries = steam::libraries(&root)?;
    let games = steam::installed_games(&libraries)?;
    let account = steam::current_account(&root).context("no Steam account found in userdata")?;
    Ok(Ctx { manifest, by_app_id, root, libraries, games, account, os: Os::current() })
}

impl Ctx {
    pub(crate) fn merged_game(&self, app_id: u64) -> Option<MergedGame> {
        let names = self.by_app_id.get(&app_id)?;
        let mut files = HashMap::new();
        let mut cloud_steam = false;
        for name in names {
            let game = &self.manifest[name];
            for (template, rule) in &game.files {
                files.insert(template.clone(), rule.clone());
            }
            cloud_steam |= game.cloud.as_ref().and_then(|c| c.steam).unwrap_or(false);
        }
        Some(MergedGame { files, cloud_steam })
    }

    /// Steam Cloud "backup-only" auto-detection (D2).
    pub(crate) fn auto_backup_only(&self, app_id: u64, merged: &MergedGame) -> bool {
        merged.cloud_steam || steam::steam_cloud_active(&self.root, app_id)
    }

    /// Does Steam itself already sync this game's saves (manifest flag or
    /// Steam's own userdata)? Independent of the user's mode setting.
    pub(crate) fn has_steam_cloud(&self, app_id: u64) -> bool {
        self.merged_game(app_id).is_some_and(|m| m.cloud_steam) || steam::steam_cloud_active(&self.root, app_id)
    }

    pub(crate) fn cloud_store(&self) -> Result<Store> {
        build_store(self.account.clone(), &config::Config::load())
    }

    /// How a game syncs: `Some(true)` backup-only, `Some(false)` two-way,
    /// `None` not at all (mode off, or no save rules).
    pub(crate) fn backup_only(&self, cfg: &config::Config, app_id: u64) -> Option<bool> {
        let merged = self.merged_game(app_id)?;
        match cfg.game(app_id).mode {
            ModeOverride::Off => None,
            ModeOverride::Sync => Some(false),
            ModeOverride::Backup => Some(true),
            ModeOverride::Auto => Some(self.auto_backup_only(app_id, &merged)),
        }
    }

    /// Modification times of everything `load_ctx` reads (manifest, local
    /// overrides, settings, Steam's account and library files, the
    /// libraries' `steamapps` folders — installing a game changes those).
    /// The daemon reloads when this changes.
    pub(crate) fn inputs_fingerprint(&self) -> Vec<Option<std::time::SystemTime>> {
        let mut paths = manifest::source_paths();
        paths.extend(config::config_path());
        paths.push(self.root.join("config/loginusers.vdf"));
        paths.push(self.root.join("steamapps/libraryfolders.vdf"));
        paths.extend(self.libraries.iter().map(|l| l.join("steamapps")));
        paths.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect()
    }
}

/// Loads several games' indexes, eight requests at a time (one after
/// another is slow on OneDrive; more than eight gets throttled).
pub(crate) fn load_indexes(store: &Store, ids: &[u64]) -> Vec<(u64, Result<Option<store::Index>>)> {
    std::thread::scope(|scope| {
        let workers: Vec<_> = ids
            .chunks(ids.len().div_ceil(8).max(1))
            .map(|chunk| scope.spawn(|| chunk.iter().map(|&id| (id, store.load_index(id))).collect::<Vec<_>>()))
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("index worker panicked")).collect()
    })
}

/// Construct a `Store` over whichever provider (D8) the user has configured
/// (`yasgm provider`); OneDrive unless a LocalFolder path is set.
pub(crate) fn build_store(account: String, cfg: &config::Config) -> Result<Store> {
    match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            // Signed-in check only; the token itself is fetched on first use.
            ensure!(onedrive::load_tokens().is_some(), "not signed in — run `yasgm auth` first");
            Store::new(Box::new(onedrive::OneDriveProvider::new()), account)
        }
        config::ProviderConfig::Local { path } => {
            Store::new(Box::new(local::LocalFolderProvider::new(path.clone())), account)
        }
        config::ProviderConfig::None => anyhow::bail!(NOT_CONNECTED),
    }
}

pub(crate) const NOT_CONNECTED: &str = "no cloud provider connected — sign in with `yasgm auth` (OneDrive) or choose a \
     folder with `yasgm provider local <path>`";

/// Steam account + cloud store without loading the manifest (for commands
/// that only touch cloud versions).
pub(crate) fn light_store() -> Result<(String, Store)> {
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::current_account(&root).context("no Steam account found in userdata")?;
    let store = build_store(account.clone(), &config::Config::load())?;
    Ok((account, store))
}

// ---- argument helpers -----------------------------------------------------
