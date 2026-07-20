//! YASGM — Yet another save game manager.
//! Commands: doctor, auth [--device], status, sync, backup, versions,
//! restore, config, pin, unpin, rm.

mod config;
mod manifest;
mod onedrive;
mod resolve;
mod snapshot;
mod steam;
mod store;
mod sync;
mod vdf;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use config::ModeOverride;
use resolve::{Os, Via};
use store::Store;

fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1_073_741_824 => format!("{:.1} GB", b as f64 / 1_073_741_824.0),
        b if b >= 1_048_576 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Everything the game-facing commands need, loaded once.
struct Ctx {
    manifest: manifest::Manifest,
    by_app_id: HashMap<u64, Vec<String>>,
    root: PathBuf,
    games: Vec<steam::InstalledGame>,
    account: String,
    os: Os,
}

/// The union of all manifest entries sharing one Steam AppID (some games have
/// several, e.g. original + Definitive Edition under the same ID).
struct MergedGame {
    files: HashMap<String, manifest::FileRule>,
    cloud_steam: bool,
}

fn load_ctx() -> Result<Ctx> {
    let manifest = manifest::load()?;
    let by_app_id = manifest::steam_index(&manifest);
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let libraries = steam::libraries(&root)?;
    let games = steam::installed_games(&libraries)?;
    let account = steam::account_ids(&root)
        .into_iter()
        .next()
        .context("no Steam account found in userdata")?;
    Ok(Ctx { manifest, by_app_id, root, games, account, os: Os::current() })
}

impl Ctx {
    fn merged_game(&self, app_id: u64) -> Option<MergedGame> {
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
    fn auto_backup_only(&self, app_id: u64, merged: &MergedGame) -> bool {
        merged.cloud_steam || steam::steam_cloud_active(&self.root, app_id)
    }

    fn cloud_store(&self) -> Result<Store> {
        let access_token = onedrive::ensure_access_token()?;
        Ok(Store::new(access_token, self.account.clone()))
    }
}

/// Steam account + cloud store without loading the manifest (for commands
/// that only touch cloud versions).
fn light_store() -> Result<(String, Store)> {
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::account_ids(&root)
        .into_iter()
        .next()
        .context("no Steam account found in userdata")?;
    let access_token = onedrive::ensure_access_token()?;
    Ok((account.clone(), Store::new(access_token, account)))
}

// ---- argument helpers -----------------------------------------------------

const VALUED_FLAGS: [&str; 3] = ["--version", "--mode", "--keep"];

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 1; // skip the command itself
    while i < args.len() {
        let arg = &args[i];
        if VALUED_FLAGS.contains(&arg.as_str()) {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

fn parse_app_id(args: &[String]) -> Option<u64> {
    positionals(args).first().and_then(|a| a.parse().ok())
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

// ---- doctor ---------------------------------------------------------------

fn doctor() -> Result<()> {
    let os = Os::current();
    println!("yasgm doctor — OS: {os:?}\n");

    let ctx = load_ctx()?;
    let cfg = config::Config::load();
    println!(
        "manifest: {} games total, {} with Steam IDs",
        ctx.manifest.len(),
        ctx.by_app_id.len()
    );
    println!("steam root: {}", ctx.root.display());
    println!(
        "installed games: {} | account: {}\n",
        ctx.games.len(),
        ctx.account
    );

    let (mut n_sync, mut n_backup, mut n_off, mut n_nodata, mut n_unknown) = (0, 0, 0, 0, 0);

    for game in &ctx.games {
        let entry = ctx.merged_game(game.app_id);
        let override_mode = cfg.game(game.app_id).mode;

        let mut lines = Vec::new();
        let mut resolved_any = false;
        if let Some(merged) = &entry {
            for (template, rule) in &merged.files {
                if !rule.is_save() {
                    continue;
                }
                let Some(res) = resolve::resolve_rule(template, rule, os, game) else {
                    continue;
                };
                let via = match res.via {
                    Via::Native => "",
                    Via::Proton => " [proton]",
                };
                match (&res.path, res.note) {
                    (Some(path), _) => {
                        let matches = resolve::existing_matches(path);
                        if matches.is_empty() {
                            lines.push(format!("    · {path}{via} (nothing on disk yet)"));
                        } else {
                            resolved_any = true;
                            for m in matches {
                                let (files, bytes) = resolve::measure(&m);
                                lines.push(format!(
                                    "    ✔ {}{via} ({files} files, {})",
                                    m.display(),
                                    human_bytes(bytes)
                                ));
                            }
                        }
                    }
                    (None, note) => {
                        let note = note.unwrap_or("unresolvable");
                        lines.push(format!("    ✘ {} — {note}", res.template));
                    }
                }
            }
        }

        let mode_label = match &entry {
            None => {
                n_unknown += 1;
                "mode: n/a (not in manifest)".to_owned()
            }
            Some(_) if lines.is_empty() => {
                n_nodata += 1;
                "mode: n/a (no save rules for this OS)".to_owned()
            }
            Some(merged) => match override_mode {
                ModeOverride::Off => {
                    n_off += 1;
                    "mode: off (user override)".to_owned()
                }
                ModeOverride::Sync => {
                    n_sync += 1;
                    "mode: sync (user override)".to_owned()
                }
                ModeOverride::Backup => {
                    n_backup += 1;
                    "mode: backup-only (user override)".to_owned()
                }
                ModeOverride::Auto => {
                    if ctx.auto_backup_only(game.app_id, merged) {
                        n_backup += 1;
                        "mode: backup-only (Steam Cloud detected)".to_owned()
                    } else {
                        n_sync += 1;
                        "mode: sync".to_owned()
                    }
                }
            },
        };
        println!("{} ({}) — {}", game.name, game.app_id, mode_label);
        for line in &lines {
            println!("{line}");
        }
        if !resolved_any && !lines.is_empty() {
            println!("    (no save data found on disk yet — game may be unplayed)");
        }
        println!();
    }

    println!(
        "summary: {n_sync} sync, {n_backup} backup-only, {n_off} off, \
         {n_nodata} without save rules, {n_unknown} not in manifest"
    );
    Ok(())
}

// ---- auth / status --------------------------------------------------------

fn auth(args: &[String]) -> Result<()> {
    let tokens = if flag(args, "--device") {
        onedrive::login_device()?
    } else {
        onedrive::login_interactive()?
    };
    onedrive::save_tokens(&tokens)?;
    println!("Sign-in successful; tokens cached.\n");
    verify_cloud(&tokens.access_token)
}

fn verify_cloud(access_token: &str) -> Result<()> {
    // Note: the AppFolder scope cannot read /me/drive (drive-wide metadata is
    // out of bounds — by design). The app folder itself is all we may touch;
    // the first request auto-creates it (Apps/YASGM).
    let approot = onedrive::graph_get(access_token, "/me/drive/special/approot")?;
    let name = approot["name"].as_str().unwrap_or("?");
    let parent = approot["parentReference"]["path"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches(':')
        .rsplit(':')
        .next()
        .unwrap_or("");
    let owner = approot["createdBy"]["user"]["displayName"].as_str().unwrap_or("?");
    let size = approot["size"].as_u64().unwrap_or(0);
    println!("App folder ready: {parent}/{name} (owner: {owner}, current size: {})", human_bytes(size));
    Ok(())
}

fn status() -> Result<()> {
    let access_token = onedrive::ensure_access_token()?;
    verify_cloud(&access_token)
}

// ---- sync -----------------------------------------------------------------

fn sync_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let only = parse_app_id(args);
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;
    let cfg = config::Config::load();
    let mut state = config::State::load();

    for game in ctx.games.iter().filter(|g| only.is_none_or(|id| id == g.app_id)) {
        let Some(merged) = ctx.merged_game(game.app_id) else {
            println!("{}: skipped (not in manifest)", game.name);
            continue;
        };
        let game_cfg = cfg.game(game.app_id);
        let backup_only = match game_cfg.mode {
            ModeOverride::Off => {
                println!("{}: skipped (mode off)", game.name);
                continue;
            }
            ModeOverride::Sync => false,
            ModeOverride::Backup => true,
            ModeOverride::Auto => ctx.auto_backup_only(game.app_id, &merged),
        };
        let keep = game_cfg.keep.unwrap_or(store::DEFAULT_KEEP);
        let last_hash = state
            .get(&ctx.account, game.app_id)
            .map(|s| s.last_hash.clone());

        let (outcome, update) = match sync::sync_game(
            &store,
            &merged.files,
            game,
            ctx.os,
            backup_only,
            last_hash.as_deref(),
            keep,
            dry_run,
        ) {
            Ok(result) => result,
            Err(e) if e.downcast_ref::<onedrive::QuotaExceeded>().is_some() => {
                eprintln!(
                    "{}: OneDrive is out of storage space — stopping sync here. Free up \
                     space (or raise your plan), then run `yasgm sync` again to pick up \
                     where this left off.",
                    game.name
                );
                break;
            }
            Err(e) => return Err(e),
        };

        match outcome {
            sync::Outcome::NothingAnywhere => {
                println!("{}: no saves locally or in cloud", game.name)
            }
            sync::Outcome::InSync => println!("{}: in sync", game.name),
            sync::Outcome::Uploaded(v) => println!(
                "{}: uploaded version {} ({} files, {})",
                game.name,
                v.id,
                v.files,
                human_bytes(v.size)
            ),
            sync::Outcome::Downloaded { version, files_written, local_preserved_as } => {
                if let Some(preserved) = local_preserved_as {
                    println!("{}: previous local state preserved as pinned version {preserved}", game.name);
                }
                println!(
                    "{}: downloaded version {} ({files_written} files restored)",
                    game.name, version.id
                );
            }
            sync::Outcome::CloudAhead(v) => println!(
                "{}: cloud has newer version {} — backup-only mode, restore manually with \
                 `yasgm restore {} --version {}`",
                game.name, v.id, game.app_id, v.id
            ),
            sync::Outcome::Conflict { uploaded, pinned } => println!(
                "{}: CONFLICT — local uploaded as active version {}; previous cloud head pinned \
                 as {} (kept until you delete it: `yasgm rm {} {}`)",
                game.name, uploaded.id, pinned, game.app_id, pinned
            ),
            sync::Outcome::DryRun(what) => println!("{}: {what}", game.name),
        }

        if let Some((version, hash)) = update {
            state.set(&ctx.account, game.app_id, &version, &hash);
        }
    }
    if !dry_run {
        state.save()?;
    }
    Ok(())
}

// ---- backup / versions / restore -----------------------------------------

fn backup_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let only = parse_app_id(args);
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;
    let cfg = config::Config::load();
    let mut state = config::State::load();

    for game in ctx.games.iter().filter(|g| only.is_none_or(|id| id == g.app_id)) {
        let Some(merged) = ctx.merged_game(game.app_id) else {
            println!("{}: skipped (not in manifest)", game.name);
            continue;
        };
        if cfg.game(game.app_id).mode == ModeOverride::Off {
            println!("{}: skipped (mode off)", game.name);
            continue;
        }
        let keep = cfg.game(game.app_id).keep.unwrap_or(store::DEFAULT_KEEP);
        match snapshot::capture(&merged.files, game, ctx.os)? {
            None => println!("{}: no save files on this machine", game.name),
            Some(snap) => {
                let index = store.load_index(game.app_id)?;
                let head = index.as_ref().and_then(Store::head);
                if head.map(|h| h.content_hash.as_str()) == Some(snap.content_hash.as_str()) {
                    println!("{}: unchanged since last version", game.name);
                    let head = head.expect("checked above");
                    state.set(&ctx.account, game.app_id, &head.id, &head.content_hash);
                    let _ = std::fs::remove_file(&snap.zip_path);
                } else if dry_run {
                    println!(
                        "{}: would upload {} files ({})",
                        game.name,
                        snap.file_count,
                        human_bytes(snap.size)
                    );
                    let _ = std::fs::remove_file(&snap.zip_path);
                } else {
                    match store.push(&game.name, game.app_id, &snap, ctx.os.name(), keep, false) {
                        Ok(version) => {
                            println!(
                                "{}: uploaded version {} ({} files, {})",
                                game.name,
                                version.id,
                                version.files,
                                human_bytes(version.size)
                            );
                            state.set(&ctx.account, game.app_id, &version.id, &version.content_hash);
                        }
                        Err(e) if e.downcast_ref::<onedrive::QuotaExceeded>().is_some() => {
                            eprintln!(
                                "{}: OneDrive is out of storage space — stopping backup here. \
                                 Free up space (or raise your plan), then run `yasgm backup` \
                                 again to pick up where this left off.",
                                game.name
                            );
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
    if !dry_run {
        state.save()?;
    }
    Ok(())
}

fn versions_cmd(args: &[String]) -> Result<()> {
    let only = parse_app_id(args);
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;

    for game in ctx.games.iter().filter(|g| only.is_none_or(|id| id == g.app_id)) {
        let Some(index) = store.load_index(game.app_id)? else {
            println!("{} ({}): no cloud versions", game.name, game.app_id);
            continue;
        };
        println!("{} ({}):", game.name, game.app_id);
        let head_id = Store::head(&index).map(|h| h.id.clone());
        let mut versions = index.versions.clone();
        versions.sort_by(|a, b| b.created.cmp(&a.created));
        for v in versions {
            let marker = if Some(&v.id) == head_id.as_ref() {
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
    Ok(())
}

fn restore_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let app_id = parse_app_id(args)
        .context("usage: yasgm restore <appid> [--version <id>] [--dry-run]")?;
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
    if let Some(merged) = ctx.merged_game(app_id) {
        if let Some(snap) = snapshot::capture(&merged.files, game, ctx.os)? {
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
    }

    let bytes = store.download_version(app_id, &version)?;
    let written = snapshot::extract(&bytes, &version.mounts, ctx.os, game, dry_run)?;
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
        state.set(&ctx.account, app_id, &head.id, &head.content_hash);
    } else {
        let new_head = store.push_raw(
            &game.name,
            app_id,
            &bytes,
            &version.content_hash,
            version.files,
            version.mounts.clone(),
            ctx.os.name(),
            keep,
            false,
        )?;
        println!("restored content re-published as active version {}", new_head.id);
        state.set(&ctx.account, app_id, &new_head.id, &new_head.content_hash);
    }
    state.save()?;
    Ok(())
}

// ---- config / pin / unpin / rm -------------------------------------------

fn config_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let mut cfg = config::Config::load();

    let Some(first) = pos.first() else {
        // List effective settings for installed games.
        let ctx = load_ctx()?;
        for game in &ctx.games {
            let game_cfg = cfg.game(game.app_id);
            let effective = match (game_cfg.mode, ctx.merged_game(game.app_id)) {
                (_, None) => "n/a (not in manifest)".to_owned(),
                (ModeOverride::Auto, Some(m)) => {
                    if ctx.auto_backup_only(game.app_id, &m) {
                        "backup-only (auto: Steam Cloud)".to_owned()
                    } else {
                        "sync (auto)".to_owned()
                    }
                }
                (mode, _) => format!("{} (override)", mode.name()),
            };
            let keep = game_cfg
                .keep
                .map(|k| k.to_string())
                .unwrap_or_else(|| format!("{} (default)", store::DEFAULT_KEEP));
            println!("{} ({}) — mode: {effective}, keep: {keep}", game.name, game.app_id);
        }
        return Ok(());
    };

    let app_id: u64 = first.parse().context("usage: yasgm config [<appid> --mode auto|sync|backup|off | --keep N | --clear]")?;
    if flag(args, "--clear") {
        cfg.games.remove(&app_id);
        cfg.save()?;
        println!("cleared overrides for {app_id}");
        return Ok(());
    }
    let mut game_cfg = cfg.game(app_id);
    if let Some(mode) = flag_value(args, "--mode") {
        game_cfg.mode = ModeOverride::parse(&mode)
            .context("--mode must be one of: auto, sync, backup, off")?;
    }
    if let Some(keep) = flag_value(args, "--keep") {
        game_cfg.keep = Some(keep.parse().context("--keep must be a number")?);
    }
    cfg.games.insert(app_id, game_cfg.clone());
    cfg.save()?;
    println!(
        "{app_id}: mode {} , keep {}",
        game_cfg.mode.name(),
        game_cfg.keep.map(|k| k.to_string()).unwrap_or_else(|| "default".into())
    );
    Ok(())
}

fn pin_cmd(args: &[String], pinned: bool) -> Result<()> {
    let pos = positionals(args);
    let (Some(app_id), Some(version_id)) = (
        pos.first().and_then(|a| a.parse::<u64>().ok()),
        pos.get(1),
    ) else {
        anyhow::bail!("usage: yasgm {} <appid> <version-id>", if pinned { "pin" } else { "unpin" });
    };
    let (_, store) = light_store()?;
    store.set_pinned(app_id, version_id, pinned)?;
    println!("{version_id}: pinned = {pinned}");
    Ok(())
}

fn rm_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let (Some(app_id), Some(version_id)) = (
        pos.first().and_then(|a| a.parse::<u64>().ok()),
        pos.get(1),
    ) else {
        anyhow::bail!("usage: yasgm rm <appid> <version-id>");
    };
    let (_, store) = light_store()?;
    store.remove_version(app_id, version_id)?;
    println!("deleted version {version_id}");
    Ok(())
}

// ---- selftest -------------------------------------------------------------

/// End-to-end roundtrip against the real cloud using a synthetic game:
/// capture → upload → unchanged-detection → mutate → upload → delete local →
/// restore → verify. Cleans up after itself.
fn selftest() -> Result<()> {
    let os = Os::current();
    let tmp = snapshot::staging_dir()?.join("selftest");
    let library = tmp.join("library");
    let save_dir = library.join("steamapps/common/SelftestGame/SavesDir");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(save_dir.join("nested"))?;
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v1")?;
    std::fs::write(save_dir.join("nested/slot2.sav"), b"nested save")?;

    let game = steam::InstalledGame {
        app_id: 999_999_999,
        name: "YASGM Selftest".to_owned(),
        install_dir: "SelftestGame".to_owned(),
        library: library.clone(),
    };
    let mut files = HashMap::new();
    files.insert(
        "<base>/SavesDir".to_owned(),
        manifest::FileRule { tags: vec!["save".to_owned()], when: Vec::new() },
    );

    let access_token = onedrive::ensure_access_token()?;
    let store = Store::new(access_token.clone(), "selftest".to_owned());

    println!("1/7 capture + upload…");
    let snap = snapshot::capture(&files, &game, os)?.context("capture found nothing")?;
    let first = store.push(&game.name, game.app_id, &snap, os.name(), 3, false)?;
    println!("      uploaded {}", first.id);

    println!("2/7 unchanged detection…");
    let snap2 = snapshot::capture(&files, &game, os)?.context("recapture found nothing")?;
    ensure!(snap2.content_hash == first.content_hash, "hash changed for identical state");
    let _ = std::fs::remove_file(&snap2.zip_path);

    println!("3/7 mutate + second version…");
    std::thread::sleep(std::time::Duration::from_secs(1)); // distinct version id
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v2 CHANGED")?;
    let snap3 = snapshot::capture(&files, &game, os)?.context("capture found nothing")?;
    ensure!(snap3.content_hash != first.content_hash, "hash identical after mutation");
    let second = store.push(&game.name, game.app_id, &snap3, os.name(), 3, false)?;
    println!("      uploaded {}", second.id);

    println!("4/7 delete local saves…");
    std::fs::remove_dir_all(&save_dir)?;

    println!("5/7 restore head from cloud…");
    let index = store.load_index(game.app_id)?.context("index missing")?;
    ensure!(index.versions.len() == 2, "expected 2 versions, found {}", index.versions.len());
    let head = Store::head(&index).context("no head")?.clone();
    ensure!(head.id == second.id, "head is not the second version");
    let bytes = store.download_version(game.app_id, &head)?;
    let written = snapshot::extract(&bytes, &head.mounts, os, &game, false)?;
    ensure!(written == 2, "expected 2 restored files, wrote {written}");

    println!("6/7 verify contents…");
    let slot1 = std::fs::read_to_string(save_dir.join("slot1.sav"))?;
    ensure!(slot1 == "selftest save v2 CHANGED", "slot1 content mismatch: {slot1:?}");
    let slot2 = std::fs::read_to_string(save_dir.join("nested/slot2.sav"))?;
    ensure!(slot2 == "nested save", "slot2 content mismatch: {slot2:?}");

    println!("7/7 sync engine matrix…");
    // In sync: nothing changed since head.
    let (outcome, update) = sync::sync_game(
        &store, &files, &game, os, false, Some(&head.content_hash), 3, false,
    )?;
    ensure!(matches!(outcome, sync::Outcome::InSync), "expected InSync, got {outcome:?}");
    let (_, head_hash) = update.context("InSync should update state")?;
    // Local edit → upload.
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v3")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    let (outcome, update) = sync::sync_game(
        &store, &files, &game, os, false, Some(&head_hash), 3, false,
    )?;
    ensure!(matches!(outcome, sync::Outcome::Uploaded(_)), "expected Uploaded, got {outcome:?}");
    let (_, v3_hash) = update.context("Uploaded should update state")?;
    // Conflict: state says we last saw something neither side has.
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v4 (divergent)")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    let (outcome, _) = sync::sync_game(
        &store, &files, &game, os, false, Some("bogus-hash-neither-side"), 3, false,
    )?;
    let sync::Outcome::Conflict { pinned, .. } = &outcome else {
        anyhow::bail!("expected Conflict, got {outcome:?}");
    };
    let index = store.load_index(game.app_id)?.context("index missing")?;
    ensure!(
        index.versions.iter().any(|v| &v.id == pinned && v.pinned),
        "conflict loser is not pinned in index"
    );
    // Cloud moved (simulated other machine): local unchanged → download.
    let local_hash = index
        .versions
        .iter()
        .find(|v| v.content_hash != v3_hash && !v.pinned)
        .map(|v| v.content_hash.clone())
        .context("no divergent head")?;
    let _ = local_hash;
    let (outcome, _) = sync::sync_game(
        &store, &files, &game, os, false, Some(&v3_hash), 3, false,
    )?;
    ensure!(
        matches!(outcome, sync::Outcome::Downloaded { .. } | sync::Outcome::InSync),
        "expected Downloaded/InSync after cloud moved, got {outcome:?}"
    );

    onedrive::delete(&access_token, "accounts/selftest")?;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("selftest passed ✔ (cloud + local artifacts cleaned up)");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("doctor") => doctor(),
        Some("auth") => auth(&args),
        Some("status") => status(),
        Some("sync") => sync_cmd(&args),
        Some("backup") => backup_cmd(&args),
        Some("versions") => versions_cmd(&args),
        Some("restore") => restore_cmd(&args),
        Some("config") => config_cmd(&args),
        Some("pin") => pin_cmd(&args, true),
        Some("unpin") => pin_cmd(&args, false),
        Some("rm") => rm_cmd(&args),
        Some("selftest") => selftest(),
        Some(other) => {
            eprintln!(
                "unknown command {other:?}\navailable:\n  \
                 doctor\n  auth [--device]\n  status\n  \
                 sync [appid] [--dry-run]\n  backup [appid] [--dry-run]\n  \
                 versions [appid]\n  restore <appid> [--version <id>] [--dry-run]\n  \
                 config [<appid> --mode auto|sync|backup|off --keep N | --clear]\n  \
                 pin <appid> <version-id>\n  unpin <appid> <version-id>\n  \
                 rm <appid> <version-id>"
            );
            std::process::exit(2);
        }
    }
}
