//! Inspecting games: `doctor`, `manifest-gaps`, `config`, `icons`, `running`,
//! `verify-update`.

use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::args::*;
use crate::config::{self, ModeOverride};
use crate::commands::saves::cached_cloud_app_ids;
use crate::ctx::*;
use crate::resolve::{self, Os, Via};
use crate::store;
use crate::{icons, manifest, running, signature, snapshot, steam};

pub(crate) fn doctor() -> Result<()> {
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
        if let Some(note) = entry.as_ref().and_then(|m| cross_os_note(&m.files, os)) {
            println!("    ⚠ cross-OS: {note}");
        }
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
    if n_unknown > 0 {
        println!(
            "hint: add entries for unlisted games to {} (same schema as the \
             community manifest; see DESIGN.md's \"Local manifest overrides\")",
            manifest::local_manifest_path()?.display()
        );
    }
    Ok(())
}

/// Plain-language note on the cross-OS restores this game can't do that
/// involve `os` (see `resolve::cross_os_gaps`), or `None` if there are none.
pub(crate) fn cross_os_note(files: &HashMap<String, manifest::FileRule>, os: Os) -> Option<String> {
    let gaps = resolve::cross_os_gaps(files);
    let into_here: Vec<_> = gaps.iter().filter(|(_, to)| *to == os).map(|(from, _)| from.label()).collect();
    let from_here: Vec<_> = gaps.iter().filter(|(from, _)| *from == os).map(|(_, to)| to.label()).collect();
    let mut parts = Vec::new();
    if !into_here.is_empty() {
        parts.push(format!("backups made on {} can't be restored here", into_here.join(" or ")));
    }
    if !from_here.is_empty() {
        parts.push(format!("backups made here can't be restored on {}", from_here.join(" or ")));
    }
    (!parts.is_empty()).then(|| format!("{} — its save locations don't match between these systems", parts.join("; ")))
}

pub(crate) const SKELETON_HEADER: &str = r#"# YASGM local manifest overrides (D7)
#
# Same format as the community Ludusavi manifest (game name → files).
# YASGM reads this file on every run and merges it over the community data:
#
#   * An entry whose `files` is empty (`files: {}`) is IGNORED. That's how
#     every game below starts; nothing changes until you fill one in.
#   * A filled entry REPLACES all of that game's save rules from the
#     community manifest (it doesn't add to them), so list every location
#     the game uses, on every OS you care about.
#   * A game name that isn't in the community manifest is added as a new game
#     (then also give `steam: {id: <appid>}` so YASGM can find it).
#
# The games below are the ones whose community save locations can't be
# matched between operating systems, so a backup made on one OS can't be
# restored on another. Each has its current community locations in comments.
# For cross-OS restore to work, name the same game folder after each OS's
# base directory, e.g. `<winAppData>/Game/Saves` and `<xdgData>/Game/Saves`.
#
# Rule format:
#   <path template>:
#     tags: [save]          # omit tags = treated as save data too
#     when: [{os: windows}] # windows | mac | linux; omit `when` = every OS
#
# Placeholders:
#   <base>                 the game's install folder (steamapps/common/<game>)
#   <root>                 the Steam library folder
#   <home>                 your home folder
#   <storeUserId>          your Steam user id (matches any id)
#   <winAppData>           Windows %APPDATA%  (…\AppData\Roaming)
#   <winLocalAppData>      Windows %LOCALAPPDATA%  (…\AppData\Local)
#   <winLocalAppDataLow>   Windows …\AppData\LocalLow
#   <winDocuments>         Windows Documents
#   <xdgData>              Linux ~/.local/share
#   <xdgConfig>            Linux ~/.config
#   macOS has no placeholder of its own: use <home>/Library/Application Support/…
#   Windows paths also apply on SteamOS/Linux inside the game's Proton prefix.
#
# Example of a filled entry:
#
# "Some Game":
#   files:
#     <winAppData>/Some Game/Saves:
#       tags: [save]
#       when: [{os: windows}]
#     <home>/Library/Application Support/Some Game/Saves:
#       tags: [save]
#       when: [{os: mac}]
#     <xdgData>/Some Game/Saves:
#       tags: [save]
#       when: [{os: linux}]
#
# Regenerate this list (without overwriting your file):
#   yasgm manifest-gaps --skeleton > ludusavi.new.yaml
"#;

/// YAML skeleton for the local manifest: one ignored placeholder entry per
/// game, with its current save locations and unmappable OS pairs as
/// comments (see `SKELETON_HEADER`).
/// A game whose save locations can't be mapped between some OS pairs.
type GapRow<'a> = (&'a String, u64, Vec<(Os, Os)>);

pub(crate) fn gap_skeleton(rows: &[GapRow], manifest: &manifest::Manifest) -> String {
    let mut out = String::from(SKELETON_HEADER);
    for (name, id, gaps) in rows {
        let pairs: Vec<_> = gaps.iter().map(|(f, t)| format!("{}→{}", f.label(), t.label())).collect();
        out.push_str(&format!("\n# {}\n# {name} (Steam {id})\n", "-".repeat(75)));
        out.push_str(&format!("# Can't restore: {}\n", pairs.join(", ")));
        out.push_str("# Community save locations:\n");
        let mut rules: Vec<(String, &String)> = manifest[*name]
            .files
            .iter()
            .filter(|(_, r)| r.is_save())
            .map(|(t, r)| {
                let mut oss: Vec<&str> = r.when.iter().filter_map(|c| c.os.as_deref()).collect();
                oss.sort();
                oss.dedup();
                let stores: Vec<&str> = r.when.iter().filter_map(|c| c.store.as_deref()).filter(|s| *s != "steam").collect();
                let mut label = if oss.is_empty() { "any OS".to_owned() } else { oss.join(",") };
                if !stores.is_empty() {
                    label.push_str(&format!(" ({} store, ignored)", stores.join(",")));
                }
                (label, t)
            })
            .collect();
        rules.sort();
        for (label, template) in rules {
            out.push_str(&format!("#   {label:<14} {template}\n"));
        }
        out.push_str(&format!("\"{}\":\n  files: {{}}\n", name.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    out
}

/// `manifest-gaps [--json|--explain|--skeleton]` (dev): every Steam game in
/// the manifest whose save rules can't be mapped between two OSes by
/// `resolve::restore_target`; `--skeleton` prints a local-manifest
/// skeleton for them.
pub(crate) fn manifest_gaps_cmd(args: &[String]) -> Result<()> {
    let manifest = manifest::load()?;
    if flag(args, "--explain") {
        let mut names: Vec<&String> = manifest.keys().collect();
        names.sort_by_key(|n| n.to_lowercase());
        for name in names {
            let game = &manifest[name];
            if game.steam.as_ref().and_then(|s| s.id).is_none() {
                continue;
            }
            for (from, to, src, dst, tier) in resolve::explain_matches(&game.files) {
                println!("{tier}\t{name}\t{}→{}\t{src}\t{dst}", from.name(), to.name());
            }
        }
        return Ok(());
    }
    let mut rows: Vec<GapRow> = manifest
        .iter()
        .filter_map(|(name, game)| {
            let id = game.steam.as_ref()?.id?;
            let gaps = resolve::cross_os_gaps(&game.files);
            (!gaps.is_empty()).then_some((name, id, gaps))
        })
        .collect();
    rows.sort_by_key(|(name, ..)| name.to_lowercase());
    if flag(args, "--skeleton") {
        print!("{}", gap_skeleton(&rows, &manifest));
        return Ok(());
    }
    if flag(args, "--json") {
        let json: Vec<_> = rows
            .iter()
            .map(|(name, id, gaps)| {
                serde_json::json!({
                    "game": name,
                    "app_id": id,
                    "gaps": gaps.iter().map(|(f, t)| format!("{}->{}", f.name(), t.name())).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }
    for (name, id, gaps) in &rows {
        let pairs: Vec<_> = gaps.iter().map(|(f, t)| format!("{}→{}", f.label(), t.label())).collect();
        println!("{name} ({id}): {}", pairs.join(", "));
    }
    let multi_os = manifest
        .values()
        .filter(|g| g.steam.as_ref().and_then(|s| s.id).is_some())
        .filter(|g| {
            [Os::Windows, Os::Mac, Os::Linux]
                .iter()
                .filter(|os| g.files.values().any(|r| r.is_save() && r.when.iter().any(|c| c.os.as_deref() == Some(os.name()))))
                .count()
                >= 2
        })
        .count();
    println!("\n{} of {multi_os} Steam games with saves on 2+ OSes have an OS pair that can't be mapped", rows.len());
    Ok(())
}

// ---- auth / status --------------------------------------------------------

/// Debug: show what the running-game detector sees right now.
pub(crate) fn running_cmd() -> Result<()> {
    let ctx = load_ctx()?;
    let mut detector = running::RunningDetector::new();
    match detector.poll(&ctx.root, &ctx.games) {
        Some(id) => println!("running: {id}"),
        None => println!("running: none"),
    }
    running::debug_dump(&ctx.games);
    Ok(())
}

// ---- backup / versions / restore -----------------------------------------

pub(crate) fn config_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let mut cfg = config::Config::load();

    let Some(first) = pos.first() else {
        // List effective settings for installed games.
        let json = flag(args, "--json");
        let ctx = load_ctx()?;
        let state = config::State::load();
        let mut json_out = Vec::new();
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
            if json {
                json_out.push(serde_json::json!({
                    "app_id": game.app_id,
                    "game": game.name,
                    "mode": game_cfg.mode.name(),
                    "effective_mode": effective,
                    "keep": game_cfg.keep,
                    "default_keep": store::DEFAULT_KEEP,
                    "in_manifest": ctx.merged_game(game.app_id).is_some(),
                    "steam_cloud": ctx.has_steam_cloud(game.app_id),
                    // null when the save location isn't known.
                    "local_saves": ctx.merged_game(game.app_id).map(|m| snapshot::has_local_saves(&m.files, game, ctx.os)),
                    "cross_os_note": ctx.merged_game(game.app_id).and_then(|m| cross_os_note(&m.files, ctx.os)),
                    "last_version": state.get(&ctx.account, game.app_id).map(|s| s.last_version.clone()),
                    "synced_at": state.get(&ctx.account, game.app_id).map(|s| s.synced_at),
                }));
                continue;
            }
            let keep = game_cfg
                .keep
                .map(|k| k.to_string())
                .unwrap_or_else(|| format!("{} (default)", store::DEFAULT_KEEP));
            println!("{} ({}) — mode: {effective}, keep: {keep}", game.name, game.app_id);
        }
        if json {
            println!("{}", serde_json::to_string(&json_out)?);
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
        game_cfg.keep = match keep.as_str() {
            "default" => None,
            n => Some(n.parse().context("--keep must be a number or `default`")?),
        };
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

/// Icon file per game: Steam's local image cache first, then (unless
/// `--offline`) a one-time download for the rest (see icons.rs). Defaults
/// to all installed games. `--embed` adds a `data_url` per entry for the
/// GUI's webview, which can't read local files.
pub(crate) fn icons_cmd(args: &[String]) -> Result<()> {
    let root = steam::find_steam_root();
    let pos = positionals(args);
    let app_ids: Vec<u64> = if pos.is_empty() {
        let root = root.as_ref().context("Steam installation not found")?;
        let mut ids: Vec<u64> = steam::installed_games(&steam::libraries(root)?)?.iter().map(|g| g.app_id).collect();
        // Plus games only in the cloud (shown as "not installed").
        ids.extend(cached_cloud_app_ids().into_iter().filter(|id| !ids.contains(id)).collect::<Vec<_>>());
        ids
    } else {
        pos.iter()
            .map(|p| p.parse().context("usage: yasgm icons [appid...] [--offline] [--json] [--embed]"))
            .collect::<Result<_>>()?
    };
    let dir = icons::cache_dir()?;
    if !flag(args, "--offline") {
        let missing: Vec<u64> =
            app_ids.iter().copied().filter(|&id| icons::resolve(root.as_deref(), &dir, id).is_none()).collect();
        if !missing.is_empty()
            && let Err(err) = icons::fetch(&dir, &missing) {
                eprintln!("icons: download skipped: {err:#}");
            }
    }
    let embed = flag(args, "--embed");
    let mut json_out = Vec::new();
    for &app_id in &app_ids {
        let path = icons::resolve(root.as_deref(), &dir, app_id);
        if flag(args, "--json") {
            let mut entry = serde_json::json!({ "app_id": app_id, "path": path });
            if let (true, Some(path)) = (embed, &path) {
                entry["data_url"] = icons::data_url(path)?.into();
            }
            json_out.push(entry);
        } else {
            println!("{app_id}\t{}", path.map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()));
        }
    }
    if flag(args, "--json") {
        println!("{}", serde_json::to_string(&json_out)?);
    }
    Ok(())
}

/// `verify-update <file> <file.sig>`: checks a release file against the
/// release key (the Deck plugin runs this before installing an update).
pub(crate) fn verify_update_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let (Some(file), Some(sig)) = (pos.first(), pos.get(1)) else {
        anyhow::bail!("usage: yasgm verify-update <file> <file.sig>");
    };
    let data = std::fs::read(file).with_context(|| format!("reading {file}"))?;
    let sig = std::fs::read_to_string(sig).with_context(|| format!("reading {sig}"))?;
    signature::verify(&data, &sig).with_context(|| format!("{file} is not a genuine YASGM release"))?;
    println!("signature ok");
    Ok(())
}

// ---- selftest -------------------------------------------------------------
