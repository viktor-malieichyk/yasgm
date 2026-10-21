//! `sync`, `backup` and the `run` launch wrapper.


use anyhow::{Context, Result, ensure};

use crate::args::*;
use crate::config;
use crate::ctx::*;
use crate::store;
use crate::{lock, onedrive, steam, sync};

pub(crate) fn sync_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let only = parse_app_id(args);
    let failed = run_sync(&load_ctx()?, only, dry_run)?.iter().filter(|e| e.kind == "error").count();
    ensure!(failed == 0, "{failed} game(s) failed to sync (see above)");
    Ok(())
}

/// Something from a sync run worth telling the user about (the daemon keeps
/// these in its `EventLog`; the Decky plugin turns them into toasts).
#[derive(Debug, Clone)]
pub struct SyncEvent {
    pub app_id: Option<u64>,
    pub game: Option<String>,
    /// `uploaded` | `downloaded` | `conflict` | `cloud_ahead` |
    /// `quota_exceeded` | `error`
    pub kind: &'static str,
    pub detail: String,
}

impl SyncEvent {
    fn for_game(game: &steam::InstalledGame, kind: &'static str, detail: String) -> SyncEvent {
        SyncEvent { app_id: Some(game.app_id), game: Some(game.name.clone()), kind, detail }
    }
}

pub(crate) fn run_sync(ctx: &Ctx, only: Option<u64>, dry_run: bool) -> Result<Vec<SyncEvent>> {
    run_sync_as(ctx, only, dry_run, false)
}

/// `run_sync`; `backup_only` treats every game as backup-only (`backup`).
pub(crate) fn run_sync_as(ctx: &Ctx, only: Option<u64>, dry_run: bool, all_backup_only: bool) -> Result<Vec<SyncEvent>> {
    // Waits for the daemon's (or another command's) sync; see lock.rs.
    let _op = if dry_run { None } else { Some(lock::acquire_operation()?) };
    let store = ctx.cloud_store()?;
    let cfg = config::Config::load();
    let mut state = config::State::load();
    let mut events = Vec::new();

    for game in ctx.games.iter().filter(|g| only.is_none_or(|id| id == g.app_id)) {
        let Some(merged) = ctx.merged_game(game.app_id) else {
            println!("{}: skipped (not in manifest)", game.name);
            continue;
        };
        let Some(backup_only) = ctx.backup_only(&cfg, game.app_id) else {
            println!("{}: skipped (mode off)", game.name);
            continue;
        };
        let backup_only = backup_only || all_backup_only;
        let keep = cfg.game(game.app_id).keep.unwrap_or(store::DEFAULT_KEEP);
        let last = state.get(&ctx.account, game.app_id);
        let job = sync::Job { store: &store, files: &merged.files, game, os: ctx.os, backup_only, keep, dry_run };
        let result = sync::sync_game(
            &job,
            last.map(|l| sync::LastSync { hash: &l.last_hash, local_hash: l.local_hash.as_deref() }),
        );
        let (outcome, update) = match result {
            Ok(result) => result,
            Err(e) if e.downcast_ref::<onedrive::QuotaExceeded>().is_some() => {
                eprintln!(
                    "{}: OneDrive is out of storage space — stopping sync here. Free up \
                     space (or raise your plan), then run `yasgm sync` again to pick up \
                     where this left off.",
                    game.name
                );
                events.push(SyncEvent::for_game(game, "quota_exceeded", "OneDrive is out of storage space".into()));
                break;
            }
            // One game failing (an unreadable or locked save file, a network
            // error) mustn't stop the rest.
            Err(e) => {
                eprintln!("{}: sync failed: {e:#}", game.name);
                events.push(SyncEvent::for_game(game, "error", format!("{e:#}")));
                continue;
            }
        };

        match outcome {
            sync::Outcome::NothingAnywhere => {
                println!("{}: no saves locally or in cloud", game.name)
            }
            sync::Outcome::InSync => println!("{}: in sync", game.name),
            sync::Outcome::Uploaded(v) => {
                let verb = if v.pending { "backed up locally (pending — offline)" } else { "uploaded" };
                println!("{}: {verb} version {} ({} files, {})", game.name, v.id, v.files, human_bytes(v.size));
                events.push(SyncEvent::for_game(game, "uploaded", if v.pending { "offline".into() } else { v.id.clone() }));
            }
            sync::Outcome::Downloaded { version, files_written, local_preserved_as } => {
                if let Some(preserved) = local_preserved_as {
                    println!("{}: previous local state preserved as pinned version {preserved}", game.name);
                }
                println!(
                    "{}: downloaded version {} ({files_written} files restored)",
                    game.name, version.id
                );
                events.push(SyncEvent::for_game(game, "downloaded", version.machine.clone()));
            }
            sync::Outcome::CloudAhead(v) => {
                println!(
                    "{}: cloud has newer version {} — backup-only mode, restore manually with \
                     `yasgm restore {} --version {}`",
                    game.name, v.id, game.app_id, v.id
                );
                events.push(SyncEvent::for_game(game, "cloud_ahead", v.machine.clone()));
            }
            sync::Outcome::Conflict { active, pinned, local_won: true, .. } => {
                let verb = if active.pending { "saved locally (pending — offline) as" } else { "active as" };
                println!(
                    "{}: CONFLICT — saves changed here and elsewhere; this machine's (newer) are {verb} \
                     version {}; the other side is pinned as {} (kept until you delete it: `yasgm rm {} {}`)",
                    game.name, active.id, pinned, game.app_id, pinned
                );
                events.push(SyncEvent::for_game(game, "conflict", pinned));
            }
            sync::Outcome::Conflict { active, pinned, local_won: false, files_written } => {
                println!(
                    "{}: CONFLICT — saves changed here and elsewhere; the newer ones from {} (version {}) \
                     are now local ({files_written} files); this machine's are pinned as {} (restore: \
                     `yasgm restore {} --version {}`)",
                    game.name, active.machine, active.id, pinned, game.app_id, pinned
                );
                events.push(SyncEvent::for_game(game, "conflict", pinned));
            }
            sync::Outcome::DryRun(what) => println!("{}: {what}", game.name),
        }

        if let Some(u) = update {
            state.set(&ctx.account, game.app_id, &u.version, &u.hash, u.local_hash.as_deref());
        }
    }
    if !dry_run {
        state.save()?;
    }
    Ok(events)
}

// ---- run (launch wrapper) --------------------------------------------------

/// Split `run` arguments into (explicit app id, game command). Everything
/// after `--` is the game command verbatim (game args may contain `--flags`
/// of their own); without `--`, the command starts at the first token that
/// isn't ours.
pub(crate) fn parse_run_args(args: &[String]) -> (Option<u64>, Vec<String>) {
    let mut app_id = None;
    let mut i = 1; // skip "run"
    while i < args.len() {
        match args[i].as_str() {
            "--" => return (app_id, args[i + 1..].to_vec()),
            "--app" => {
                app_id = args.get(i + 1).and_then(|a| a.parse().ok());
                i += 2;
            }
            _ => return (app_id, args[i..].to_vec()),
        }
    }
    (app_id, Vec::new())
}

/// Steam launch options integration: `yasgm run -- %command%`.
/// Sync before the game reads its saves, run it, sync after it exits.
/// A failed sync never blocks the game from launching (worst case the user
/// plays on local saves and the post-exit sync reconciles as a conflict —
/// non-destructive either way).
pub(crate) fn run_cmd(args: &[String]) -> Result<()> {
    let (explicit_app, command) = parse_run_args(args);
    anyhow::ensure!(
        !command.is_empty(),
        "usage: yasgm run [--app <appid>] -- <game command...>"
    );
    // Steam exports SteamAppId to the launched process tree.
    let app_id = explicit_app
        .or_else(|| std::env::var("SteamAppId").ok().and_then(|v| v.parse().ok()))
        .or_else(|| std::env::var("SteamGameId").ok().and_then(|v| v.parse().ok()));

    let ctx = match app_id.map(|_| load_ctx()) {
        Some(Ok(ctx)) => Some(ctx),
        Some(Err(err)) => {
            eprintln!("yasgm: can't sync (launching anyway): {err:#}");
            None
        }
        None => None,
    };
    match (app_id, &ctx) {
        (Some(id), Some(ctx)) => {
            eprintln!("yasgm: pre-launch sync (app {id})…");
            if let Err(err) = run_sync(ctx, Some(id), false) {
                eprintln!("yasgm: pre-launch sync failed (launching anyway): {err:#}");
            }
        }
        (Some(_), None) => {}
        (None, _) => eprintln!(
            "yasgm: no app id (no SteamAppId in environment and no --app flag) — \
             launching without sync"
        ),
    }

    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| format!("launching {:?}", command[0]))?;

    if let (Some(id), Some(ctx)) = (app_id, &ctx) {
        eprintln!("yasgm: post-exit sync (app {id})…");
        if let Err(err) = run_sync(ctx, Some(id), false) {
            eprintln!("yasgm: post-exit sync failed: {err:#}");
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

// ---- watch (daemon) --------------------------------------------------------

/// `backup [appid]`: a sync that never downloads — every game is treated
/// as backup-only (D2): local changes are uploaded, newer cloud saves are
/// only reported, and in a conflict the local saves become active with the
/// cloud's pinned.
pub(crate) fn backup_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let only = parse_app_id(args);
    let failed = run_sync_as(&load_ctx()?, only, dry_run, true)?.iter().filter(|e| e.kind == "error").count();
    ensure!(failed == 0, "{failed} game(s) failed to back up (see above)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_run_args;


    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    #[test]
    fn run_args_with_separator_keeps_game_flags() {
        let (app, cmd) = parse_run_args(&s(&["run", "--app", "42", "--", "game.exe", "--fullscreen", "--app"]));
        assert_eq!(app, Some(42));
        assert_eq!(cmd, s(&["game.exe", "--fullscreen", "--app"]));
    }
    #[test]
    fn run_args_without_separator() {
        let (app, cmd) = parse_run_args(&s(&["run", "game.exe", "--app", "7"]));
        assert_eq!(app, None);
        assert_eq!(cmd, s(&["game.exe", "--app", "7"]));
    }
    #[test]
    fn run_args_empty_command() {
        let (app, cmd) = parse_run_args(&s(&["run", "--app", "42"]));
        assert_eq!(app, Some(42));
        assert!(cmd.is_empty());
    }
}
