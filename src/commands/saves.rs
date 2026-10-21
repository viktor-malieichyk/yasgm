//! Cloud versions: `versions`, `restore`, `pin`/`unpin`/`rm`, `cloud-games`,
//! `move-account`, and the folder lookups the GUI opens.


use anyhow::{Context, Result};

use crate::args::*;
use crate::config;
use crate::ctx::*;
use crate::resolve::{self, Os};
use crate::store::{self, Store};
use crate::{fsutil, lock, onedrive, snapshot, steam, sync};

/// Real, existing local save directories for one installed game — the
/// subset of `doctor`'s per-game resolution that has data on disk, as JSON
/// (used by the GUI's "Open save location" button).
pub(crate) fn paths_cmd(args: &[String]) -> Result<()> {
    let app_id = parse_app_id(args).context("usage: yasgm paths <appid>")?;
    let ctx = load_ctx()?;
    let game = ctx
        .games
        .iter()
        .find(|g| g.app_id == app_id)
        .context("game is not installed on this machine")?;
    let merged = ctx.merged_game(app_id).context("game is not in the manifest")?;

    let mut paths = Vec::new();
    for (template, rule) in &merged.files {
        if !rule.is_save() {
            continue;
        }
        let Some(res) = resolve::resolve_rule(template, rule, ctx.os, game) else {
            continue;
        };
        let Some(path) = &res.path else { continue };
        for m in resolve::existing_matches(path) {
            let (files, bytes) = resolve::measure(&m);
            paths.push(serde_json::json!({"path": m, "files": files, "bytes": bytes}));
        }
    }
    println!("{}", serde_json::to_string(&paths)?);
    Ok(())
}

/// Where this game's backups actually live on the *primary* provider (D8) —
/// a local path for LocalFolder, or a real OneDrive web link for OneDrive
/// (fetched via Graph's `webUrl`, since there's no local folder to open;
/// D13). Used by the GUI's "Open in cloud provider" button; the local
/// branch also creates the directory so opening it never fails on absence.
pub(crate) fn backup_location_cmd(args: &[String]) -> Result<()> {
    let app_id = parse_app_id(args).context("usage: yasgm backup-location <appid>")?;
    let cfg = config::Config::load();
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::current_account(&root).context("no Steam account found in userdata")?;
    let json = match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            let session = onedrive::Session::new();
            let rel = format!("accounts/{account}/games/{app_id}");
            let web_url = session.item_get(&rel)?
                .and_then(|item| item["webUrl"].as_str().map(str::to_owned));
            // No backups made yet for this game: fall back to the app's
            // OneDrive root folder (always exists — Graph auto-creates it)
            // rather than a dead end.
            let web_url = match web_url {
                Some(url) => Some(url),
                None => session.graph_get("/me/drive/special/approot")?
                    ["webUrl"]
                    .as_str()
                    .map(str::to_owned),
            };
            serde_json::json!({"kind": "cloud", "url": web_url})
        }
        config::ProviderConfig::Local { path } => {
            let game_dir = path.join(format!("accounts/{account}/games/{app_id}"));
            std::fs::create_dir_all(&game_dir)
                .with_context(|| format!("creating {}", game_dir.display()))?;
            serde_json::json!({"kind": "local", "path": game_dir})
        }
        config::ProviderConfig::None => anyhow::bail!(NOT_CONNECTED),
    };
    println!("{}", serde_json::to_string(&json)?);
    Ok(())
}

/// Where any offline-pending (unsynced) backups for this game currently
/// live (D18) — a fixed local path independent of whichever cloud provider
/// is configured, since the pending store always exists as a fallback.
/// Used by the GUI's "Open backups location" button, shown only when this
/// game actually has pending versions (see `versions --json`'s `pending`
/// field) rather than unconditionally.
pub(crate) fn pending_location_cmd(args: &[String]) -> Result<()> {
    let app_id = parse_app_id(args).context("usage: yasgm pending-location <appid>")?;
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::current_account(&root).context("no Steam account found in userdata")?;
    let dir = store::pending_dir()?.join(format!("accounts/{account}/games/{app_id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    println!("{}", serde_json::to_string(&serde_json::json!({"path": dir}))?);
    Ok(())
}

// ---- sync -----------------------------------------------------------------

pub(crate) fn versions_cmd(args: &[String]) -> Result<()> {
    let only = parse_app_id(args);
    let json = flag(args, "--json");
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;
    let mut json_out = Vec::new();

    // Installed games, or the one asked for even if it isn't installed here
    // (its name then comes from the cloud index).
    let mut targets: Vec<(u64, String)> = ctx
        .games
        .iter()
        .filter(|g| only.is_none_or(|id| id == g.app_id))
        .map(|g| (g.app_id, g.name.clone()))
        .collect();
    if let (Some(id), true) = (only, targets.is_empty()) {
        targets.push((id, String::new()));
    }
    for (app_id, name) in targets {
        let Some(index) = store.load_index(app_id)? else {
            if !json {
                println!("{name} ({app_id}): no cloud versions");
            }
            continue;
        };
        let name = if name.is_empty() { index.game.clone() } else { name };
        if !json {
            println!("{name} ({app_id}):");
        }
        let head_id = Store::head(&index).map(|h| h.id.clone());
        let mut versions = index.versions.clone();
        versions.sort_by_key(|v| std::cmp::Reverse(v.created));
        for v in versions {
            let active = Some(&v.id) == head_id.as_ref();
            if json {
                json_out.push(serde_json::json!({
                    "app_id": app_id,
                    "game": name,
                    "id": v.id,
                    "created": v.created,
                    "machine": v.machine,
                    "os": v.os,
                    "files": v.files,
                    "size": v.size,
                    "pinned": v.pinned,
                    "active": active,
                    "pending": v.pending,
                }));
                continue;
            }
            let marker = if v.pending {
                " [pending — offline, not yet synced]"
            } else if active {
                " [active]"
            } else if v.pinned {
                " [pinned]"
            } else {
                ""
            };
            println!(
                "  {} — {} files, {}, from {} ({}){marker}",
                v.id,
                v.files,
                human_bytes(v.size),
                v.machine,
                v.os
            );
        }
    }
    if json {
        println!("{}", serde_json::to_string(&json_out)?);
    }
    Ok(())
}

/// Games with saves in the cloud that aren't installed on this machine
/// (backed up from another PC or Deck), with their newest version.
/// Takes a few seconds (one request per game), so every run also saves the
/// result and `--cached` prints that instantly (`[]` before the first run):
/// the GUI and the Deck panel show it first, then refresh.
pub(crate) fn cloud_games_cache() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("yasgm/cloud-games.json"))
}

/// App IDs from the last `cloud-games` run (no network).
pub(crate) fn cached_cloud_app_ids() -> Vec<u64> {
    let text = cloud_games_cache().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    serde_json::from_str::<Vec<serde_json::Value>>(&text)
        .unwrap_or_default()
        .iter()
        .filter_map(|g| g["app_id"].as_u64())
        .collect()
}

pub(crate) fn cloud_games_cmd(args: &[String]) -> Result<()> {
    let cache = cloud_games_cache();
    if flag(args, "--cached") {
        let cached = cache.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        println!("{}", cached.as_deref().map(str::trim).unwrap_or("[]"));
        return Ok(());
    }
    let account_flag = flag_value(args, "--account");
    let store = match &account_flag {
        Some(account) => build_store(account.clone(), &config::Config::load())?,
        None => light_store()?.1,
    };
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let installed: std::collections::HashSet<u64> =
        steam::installed_games(&steam::libraries(&root)?)?.iter().map(|g| g.app_id).collect();
    let ids: Vec<u64> = store.cloud_app_ids()?.into_iter().filter(|id| !installed.contains(id)).collect();
    let indexes: Vec<store::Index> = load_indexes(&store, &ids)
        .into_iter()
        .map(|(_, index)| index)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut out = Vec::new();
    for index in indexes {
        let app_id = index.app_id;
        let latest = index.versions.iter().max_by_key(|v| v.created);
        out.push(serde_json::json!({
            "app_id": app_id,
            "game": index.game,
            "versions": index.versions.len(),
            "latest_at": latest.map(|v| v.created),
            "latest_machine": latest.map(|v| v.machine.clone()),
            "latest_os": latest.map(|v| v.os.clone()),
        }));
    }
    out.sort_by_key(|g| g["game"].as_str().unwrap_or_default().to_lowercase());
    // Saves are kept per Steam account (D4); say so if another account has some.
    for (account, games) in store.other_accounts().unwrap_or_default() {
        eprintln!("note: Steam account {account} also has saves for {games} game(s) in the cloud (not shown)");
    }
    let json = serde_json::to_string(&out)?;
    if let (Some(path), None) = (&cache, &account_flag) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = fsutil::write_atomic(path, json.as_bytes());
    }
    if flag(args, "--json") {
        println!("{json}");
    } else if out.is_empty() {
        println!("no cloud saves for games that aren't installed here");
    } else {
        for g in &out {
            println!(
                "{} ({}) — {} version(s), newest from {} ({})",
                g["game"].as_str().unwrap_or("?"),
                g["app_id"],
                g["versions"],
                g["latest_machine"].as_str().unwrap_or("?"),
                g["latest_os"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

/// `move-account <from> [--to <account>] [--dry-run] [--keep-source]`:
/// moves every game's cloud saves from one Steam account's folder into
/// another (default: the one signed in here) — for saves a device uploaded
/// under the wrong account. Copies are verified before anything is deleted;
/// where both accounts have the game, the newer saves become active and the
/// older stay pinned.
pub(crate) fn move_account_cmd(args: &[String]) -> Result<()> {
    let usage = "usage: yasgm move-account <from-account> [--to <account>] [--dry-run] [--keep-source]";
    let from = positionals(args).first().cloned().context(usage)?;
    let to = match flag_value(args, "--to") {
        Some(to) => to,
        None => light_store()?.0,
    };
    anyhow::ensure!(from != to, "both accounts are {to}");
    let dry_run = flag(args, "--dry-run");
    // Waits for the daemon's (or another command's) sync; see lock.rs.
    let _op = if dry_run { None } else { Some(lock::acquire_operation()?) };
    let cfg = config::Config::load();
    let (source, dest) = (build_store(from.clone(), &cfg)?, build_store(to.clone(), &cfg)?);
    let games = source.cloud_app_ids()?;
    anyhow::ensure!(!games.is_empty(), "account {from} has no saves in the cloud");
    println!("{} {} game(s) from account {from} to {to}…", if dry_run { "would move" } else { "moving" }, games.len());
    let mut moved = 0;
    for app_id in &games {
        let copied = dest.import_game(&source, *app_id, dry_run)?;
        let name = dest.load_index(*app_id)?.map(|i| i.game).or(source.load_index(*app_id)?.map(|i| i.game));
        println!("  {} ({app_id}): {copied} version(s)", name.unwrap_or_default());
        if !dry_run && !flag(args, "--keep-source") {
            source.delete_game(*app_id)?;
        }
        moved += 1;
    }
    if !dry_run && !flag(args, "--keep-source") {
        source.delete_account_folder();
    }
    println!("{} {moved} game(s)", if dry_run { "dry run: would move" } else { "moved" });
    Ok(())
}

pub(crate) fn restore_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let app_id = parse_app_id(args)
        .context("usage: yasgm restore <appid> [--version <id>] [--dry-run]")?;
    // Waits for the daemon's (or another command's) sync; see lock.rs.
    let _op = if dry_run { None } else { Some(lock::acquire_operation()?) };
    let ctx = load_ctx()?;
    let game = ctx
        .games
        .iter()
        .find(|g| g.app_id == app_id)
        .context("game is not installed on this machine")?;
    let store = ctx.cloud_store()?;
    let index = store
        .load_index(app_id)?
        .context("no cloud versions exist for this game")?;
    let version = match flag_value(args, "--version") {
        Some(id) => index
            .versions
            .iter()
            .find(|v| v.id == id)
            .with_context(|| format!("version {id} not found (see `yasgm versions {app_id}`)"))?
            .clone(),
        None => Store::head(&index).context("no active version in index")?.clone(),
    };
    let keep = config::Config::load().game(app_id).keep.unwrap_or(store::DEFAULT_KEEP);
    let mut state = config::State::load();

    // Non-destructive restore (D14): preserve the current local state as a
    // *pinned* version before overwriting anything (pinned so it never
    // competes with the sync head).
    if let Some(merged) = ctx.merged_game(app_id)
        && let Some(snap) = snapshot::capture(&merged.files, game, ctx.os)? {
            if snap.content_hash == version.content_hash {
                println!("local saves already match version {}; nothing to do", version.id);
                let _ = std::fs::remove_file(&snap.zip_path);
                return Ok(());
            }
            if dry_run {
                println!("would preserve current local state as a pinned version first");
                let _ = std::fs::remove_file(&snap.zip_path);
            } else {
                let safety =
                    store.push(&game.name, app_id, &snap, ctx.os.name(), keep, true)?;
                println!("current local state preserved as pinned version {}", safety.id);
            }
        }

    let bytes = store.download_version(app_id, &version)?;
    let files = ctx.merged_game(app_id).map(|m| m.files).unwrap_or_default();
    if let Some(from) = Os::from_name(&version.os).filter(|from| *from != ctx.os)
        && resolve::cross_os_gaps(&files).contains(&(from, ctx.os)) {
            eprintln!(
                "warning: {} was made on {}, and this game's save locations on {} and {} don't match — \
                 its files will likely be skipped",
                version.id,
                from.label(),
                from.label(),
                ctx.os.label()
            );
        }
    let written = snapshot::extract(&bytes, &version, &files, ctx.os, game, dry_run)?;
    if dry_run {
        println!("dry run: {written} files would be restored from {}", version.id);
        return Ok(());
    }
    println!("restored {written} files from version {}", version.id);

    // Make the restored content the active head, or sync would immediately
    // "update" us back to whatever was newer.
    let index = store.load_index(app_id)?.context("index vanished")?;
    let already_head = Store::head(&index)
        .is_some_and(|h| h.content_hash == version.content_hash);
    if already_head {
        let head = Store::head(&index).expect("checked above");
        let local = sync::local_hash(&files, game, ctx.os)?;
        state.set(&ctx.account, app_id, &head.id, &head.content_hash, local.as_deref().filter(|h| *h != head.content_hash));
    } else {
        let new_head = store.push_raw(
            &store::NewVersion {
                game: &game.name,
                app_id,
                bytes: &bytes,
                content_hash: &version.content_hash,
                files: version.files,
                mounts: version.mounts.clone(),
                os: ctx.os.name(),
                saved_at: version.saved_at,
                pinned: false,
            },
            keep,
        )?;
        println!("restored content re-published as active version {}", new_head.id);
        if !new_head.pending {
            let local = sync::local_hash(&files, game, ctx.os)?;
            state.set(&ctx.account, app_id, &new_head.id, &new_head.content_hash, local.as_deref().filter(|h| *h != new_head.content_hash));
        }
    }
    state.save()?;
    Ok(())
}

// ---- config / pin / unpin / rm -------------------------------------------

pub(crate) fn pin_cmd(args: &[String], pinned: bool) -> Result<()> {
    let pos = positionals(args);
    let (Some(app_id), Some(version_id)) = (
        pos.first().and_then(|a| a.parse::<u64>().ok()),
        pos.get(1),
    ) else {
        anyhow::bail!("usage: yasgm {} <appid> <version-id>", if pinned { "pin" } else { "unpin" });
    };
    let _op = lock::acquire_operation()?;
    let (_, store) = light_store()?;
    store.set_pinned(app_id, version_id, pinned)?;
    println!("{version_id}: pinned = {pinned}");
    Ok(())
}

pub(crate) fn rm_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let (Some(app_id), Some(version_id)) = (
        pos.first().and_then(|a| a.parse::<u64>().ok()),
        pos.get(1),
    ) else {
        anyhow::bail!("usage: yasgm rm <appid> <version-id>");
    };
    let _op = lock::acquire_operation()?;
    let (_, store) = light_store()?;
    store.remove_version(app_id, version_id)?;
    println!("deleted version {version_id}");
    Ok(())
}
