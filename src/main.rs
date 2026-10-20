//! YASGM — Yet another save game manager.
//! Commands: doctor, auth [--device], status, sync, backup, versions,
//! restore, config, pin, unpin, rm.

mod autostart;
mod config;
mod control;
mod fsutil;
mod gui;
mod icons;
mod local;
mod lock;
mod manifest;
mod onedrive;
mod provider;
mod resolve;
mod running;
mod snapshot;
mod steam;
mod store;
mod sync;
mod tray;
mod vdf;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
        build_store(self.account.clone(), &config::Config::load())
    }
}

/// Construct a `Store` over whichever provider (D8) the user has configured
/// (`yasgm provider`); OneDrive unless a LocalFolder path is set.
fn build_store(account: String, cfg: &config::Config) -> Result<Store> {
    match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            let access_token = onedrive::ensure_access_token()?;
            Store::new(Box::new(onedrive::OneDriveProvider::new(access_token)), account)
        }
        config::ProviderConfig::Local { path } => {
            Store::new(Box::new(local::LocalFolderProvider::new(path.clone())), account)
        }
        config::ProviderConfig::None => anyhow::bail!(NOT_CONNECTED),
    }
}

const NOT_CONNECTED: &str = "no cloud provider connected — sign in with `yasgm auth` (OneDrive) or choose a \
     folder with `yasgm provider local <path>`";

/// Steam account + cloud store without loading the manifest (for commands
/// that only touch cloud versions).
fn light_store() -> Result<(String, Store)> {
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::account_ids(&root)
        .into_iter()
        .next()
        .context("no Steam account found in userdata")?;
    let store = build_store(account.clone(), &config::Config::load())?;
    Ok((account, store))
}

// ---- argument helpers -----------------------------------------------------

const VALUED_FLAGS: [&str; 5] = ["--version", "--mode", "--keep", "--app", "--settle"];

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
fn cross_os_note(files: &HashMap<String, manifest::FileRule>, os: Os) -> Option<String> {
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

const SKELETON_HEADER: &str = r#"# YASGM local manifest overrides (D7)
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
fn gap_skeleton(rows: &[(&String, u64, Vec<(Os, Os)>)], manifest: &manifest::Manifest) -> String {
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
fn manifest_gaps_cmd(args: &[String]) -> Result<()> {
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
    let mut rows: Vec<(&String, u64, Vec<(Os, Os)>)> = manifest
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

fn auth(args: &[String]) -> Result<()> {
    if let config::ProviderConfig::Local { path } = config::Config::load().provider {
        if flag(args, "--json") {
            println!("{}", serde_json::json!({"event": "not_needed", "detail": format!("cloud provider is a local folder ({})", path.display())}));
        } else {
            println!("cloud provider is a local folder ({}); no sign-in needed.", path.display());
        }
        return Ok(());
    }
    if flag(args, "--device") && flag(args, "--json") {
        return auth_device_json();
    }
    let tokens = if flag(args, "--device") {
        onedrive::login_device(&|event| match event {
            onedrive::DeviceEvent::Code { user_code, verification_uri, message, .. } => {
                match message {
                    Some(message) => println!("{message}"),
                    None => println!("Visit {verification_uri} on any device and enter the code {user_code}"),
                }
                // Scan with a phone instead of typing the link.
                if let Ok(code) = qrcode::QrCode::new(verification_uri.as_bytes()) {
                    println!(
                        "\n{}\n",
                        code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build()
                    );
                }
            }
            onedrive::DeviceEvent::Waiting { elapsed } => eprintln!("  still waiting for sign-in… ({elapsed}s elapsed)"),
        })?
    } else {
        onedrive::login_interactive()?
    };
    onedrive::save_tokens(&tokens)?;
    connect_onedrive()?;
    println!("Sign-in successful; tokens cached.\n");
    verify_cloud(&tokens.access_token)
}

/// Signing in is choosing OneDrive: a disconnected install becomes a
/// OneDrive one (a LocalFolder install never gets here — `auth` refuses).
fn connect_onedrive() -> Result<()> {
    let mut cfg = config::Config::load();
    if matches!(cfg.provider, config::ProviderConfig::None) {
        cfg.provider = config::ProviderConfig::Onedrive;
        cfg.save()?;
    }
    Ok(())
}

/// `auth --device --json`: one JSON object per line, for a UI (the Decky
/// plugin shows the code and a QR of the URL): `{"event":"code",
/// "user_code","verification_uri","expires_in"}`, then `{"event":"waiting",
/// "elapsed"}` every 30 s, then `{"event":"signed_in","detail"}` or
/// `{"event":"error","message"}` (exit status 1).
fn auth_device_json() -> Result<()> {
    let emit = |value: serde_json::Value| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    };
    let result = onedrive::login_device(&|event| match event {
        onedrive::DeviceEvent::Code { user_code, verification_uri, expires_in, .. } => emit(serde_json::json!({
            "event": "code",
            "user_code": user_code,
            "verification_uri": verification_uri,
            "expires_in": expires_in,
        })),
        onedrive::DeviceEvent::Waiting { elapsed } => emit(serde_json::json!({"event": "waiting", "elapsed": elapsed})),
    })
    .and_then(|tokens| {
        onedrive::save_tokens(&tokens)?;
        connect_onedrive()?;
        cloud_summary(&tokens.access_token)
    });
    match result {
        Ok(detail) => {
            emit(serde_json::json!({"event": "signed_in", "detail": detail}));
            Ok(())
        }
        Err(err) => {
            emit(serde_json::json!({"event": "error", "message": format!("{err:#}")}));
            std::process::exit(1);
        }
    }
}

fn verify_cloud(access_token: &str) -> Result<()> {
    println!("{}", cloud_summary(access_token)?);
    Ok(())
}

/// One line describing the OneDrive app folder, proving the token works.
fn cloud_summary(access_token: &str) -> Result<String> {
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
    Ok(format!("App folder ready: {parent}/{name} (owner: {owner}, current size: {})", human_bytes(size)))
}

/// `status [--json]`: is the configured provider reachable and writable,
/// and is the watch daemon running. With `--json` a failed check is
/// reported in the output (`"ok": false, "error"`) rather than as an error.
fn status(args: &[String]) -> Result<()> {
    let cfg = config::Config::load();
    if !flag(args, "--json") {
        println!("{}", check_provider(&cfg.provider)?);
        return Ok(());
    }
    let check = check_provider(&cfg.provider);
    let daemon = lock::daemon_status()?;
    let (provider, signed_in) = match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            (serde_json::json!({"type": "onedrive"}), serde_json::json!(onedrive::load_tokens().is_some()))
        }
        config::ProviderConfig::Local { path } => {
            (serde_json::json!({"type": "local", "path": path}), serde_json::Value::Null)
        }
        config::ProviderConfig::None => (serde_json::json!({"type": "none"}), serde_json::Value::Null),
    };
    println!(
        "{}",
        serde_json::json!({
            "provider": provider,
            "signed_in": signed_in,
            "ok": check.is_ok(),
            "detail": check.as_ref().ok(),
            "error": check.as_ref().err().map(|err| format!("{err:#}")),
            "daemon": {"running": daemon.is_some(), "pid": daemon.flatten()},
        })
    );
    Ok(())
}

/// `Ok(summary)` if the configured provider is reachable and writable.
fn check_provider(provider: &config::ProviderConfig) -> Result<String> {
    match provider {
        config::ProviderConfig::None => anyhow::bail!(NOT_CONNECTED),
        config::ProviderConfig::Onedrive => cloud_summary(&onedrive::ensure_access_token()?),
        config::ProviderConfig::Local { path } => {
            let meta = std::fs::metadata(&path)
                .with_context(|| format!("{} not accessible", path.display()))?;
            ensure!(meta.is_dir(), "{} is not a directory", path.display());
            let probe = path.join(".yasgm-write-test");
            std::fs::write(&probe, b"ok")
                .with_context(|| format!("{} is not writable", path.display()))?;
            let _ = std::fs::remove_file(&probe);
            Ok(format!("Local folder provider ready: {}", path.display()))
        }
    }
}

/// Select or inspect the cloud provider (D8).
fn provider_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let mut cfg = config::Config::load();
    match pos.first().map(String::as_str) {
        Some("onedrive") => {
            cfg.provider = config::ProviderConfig::Onedrive;
            cfg.save()?;
            println!("cloud provider set to OneDrive");
        }
        Some("disconnect") => {
            let was = std::mem::replace(&mut cfg.provider, config::ProviderConfig::None);
            cfg.save()?;
            let detail = match was {
                config::ProviderConfig::Onedrive => {
                    onedrive::delete_tokens()?;
                    "signed out of OneDrive on this device. Your saves in OneDrive are untouched; to also revoke \
                     YASGM's access, visit https://account.live.com/consent/Manage"
                        .to_owned()
                }
                config::ProviderConfig::Local { path } => {
                    format!("stopped using the folder {} (its contents are untouched)", path.display())
                }
                config::ProviderConfig::None => "no cloud provider was connected".to_owned(),
            };
            if flag(args, "--json") {
                println!("{}", serde_json::json!({"type": "none", "detail": detail}));
            } else {
                println!("disconnected: {detail}");
            }
        }
        Some("local") => {
            let raw = pos.get(1).context("usage: yasgm provider local <path>")?;
            std::fs::create_dir_all(raw).with_context(|| format!("creating {raw}"))?;
            let path = std::fs::canonicalize(raw)?;
            cfg.provider = config::ProviderConfig::Local { path: path.clone() };
            cfg.save()?;
            println!("cloud provider set to local folder: {}", path.display());
        }
        None | Some("status") => {
            if flag(args, "--json") {
                let json = match &cfg.provider {
                    config::ProviderConfig::Onedrive => serde_json::json!({"type": "onedrive"}),
                    config::ProviderConfig::Local { path } => {
                        serde_json::json!({"type": "local", "path": path})
                    }
                    config::ProviderConfig::None => serde_json::json!({"type": "none"}),
                };
                println!("{}", serde_json::to_string(&json)?);
            } else {
                match &cfg.provider {
                    config::ProviderConfig::Onedrive => println!("cloud provider: OneDrive"),
                    config::ProviderConfig::Local { path } => {
                        println!("cloud provider: local folder at {}", path.display())
                    }
                    config::ProviderConfig::None => println!("cloud provider: none (disconnected)"),
                }
            }
        }
        Some(other) => {
            anyhow::bail!("usage: yasgm provider [onedrive|local <path>|disconnect|status] (got {other:?})")
        }
    }
    Ok(())
}

/// Real, existing local save directories for one installed game — the
/// subset of `doctor`'s per-game resolution that has data on disk, as JSON
/// (used by the GUI's "Open save location" button).
fn paths_cmd(args: &[String]) -> Result<()> {
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
fn backup_location_cmd(args: &[String]) -> Result<()> {
    let app_id = parse_app_id(args).context("usage: yasgm backup-location <appid>")?;
    let cfg = config::Config::load();
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::account_ids(&root)
        .into_iter()
        .next()
        .context("no Steam account found in userdata")?;
    let json = match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            let access_token = onedrive::ensure_access_token()?;
            let rel = format!("accounts/{account}/games/{app_id}");
            let web_url = onedrive::item_get(&access_token, &rel)?
                .and_then(|item| item["webUrl"].as_str().map(str::to_owned));
            // No backups made yet for this game: fall back to the app's
            // OneDrive root folder (always exists — Graph auto-creates it)
            // rather than a dead end.
            let web_url = match web_url {
                Some(url) => Some(url),
                None => onedrive::graph_get(&access_token, "/me/drive/special/approot")?
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
fn pending_location_cmd(args: &[String]) -> Result<()> {
    let app_id = parse_app_id(args).context("usage: yasgm pending-location <appid>")?;
    let root = steam::find_steam_root().context("Steam installation not found")?;
    let account = steam::account_ids(&root)
        .into_iter()
        .next()
        .context("no Steam account found in userdata")?;
    let dir = store::pending_dir()?.join(format!("accounts/{account}/games/{app_id}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    println!("{}", serde_json::to_string(&serde_json::json!({"path": dir}))?);
    Ok(())
}

// ---- sync -----------------------------------------------------------------

fn sync_cmd(args: &[String]) -> Result<()> {
    let dry_run = flag(args, "--dry-run");
    let only = parse_app_id(args);
    run_sync(only, dry_run).map(drop)
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

fn run_sync(only: Option<u64>, dry_run: bool) -> Result<Vec<SyncEvent>> {
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;
    let cfg = config::Config::load();
    let mut state = config::State::load();
    let mut events = Vec::new();

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
                events.push(SyncEvent::for_game(game, "quota_exceeded", "OneDrive is out of storage space".into()));
                break;
            }
            Err(e) => return Err(e),
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
            sync::Outcome::Conflict { uploaded, pinned } => {
                let verb = if uploaded.pending { "saved locally (pending — offline) as" } else { "uploaded as" };
                println!(
                    "{}: CONFLICT — local {verb} active version {}; previous cloud head pinned \
                     as {} (kept until you delete it: `yasgm rm {} {}`)",
                    game.name, uploaded.id, pinned, game.app_id, pinned
                );
                events.push(SyncEvent::for_game(game, "conflict", pinned));
            }
            sync::Outcome::DryRun(what) => println!("{}: {what}", game.name),
        }

        if let Some((version, hash)) = update {
            state.set(&ctx.account, game.app_id, &version, &hash);
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
fn parse_run_args(args: &[String]) -> (Option<u64>, Vec<String>) {
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
fn run_cmd(args: &[String]) -> Result<()> {
    let (explicit_app, command) = parse_run_args(args);
    anyhow::ensure!(
        !command.is_empty(),
        "usage: yasgm run [--app <appid>] -- <game command...>"
    );
    // Steam exports SteamAppId to the launched process tree.
    let app_id = explicit_app
        .or_else(|| std::env::var("SteamAppId").ok().and_then(|v| v.parse().ok()))
        .or_else(|| std::env::var("SteamGameId").ok().and_then(|v| v.parse().ok()));

    match app_id {
        Some(id) => {
            eprintln!("yasgm: pre-launch sync (app {id})…");
            if let Err(err) = run_sync(Some(id), false) {
                eprintln!("yasgm: pre-launch sync failed (launching anyway): {err:#}");
            }
        }
        None => eprintln!(
            "yasgm: no app id (no SteamAppId in environment and no --app flag) — \
             launching without sync"
        ),
    }

    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| format!("launching {:?}", command[0]))?;

    if let Some(id) = app_id {
        eprintln!("yasgm: post-exit sync (app {id})…");
        if let Err(err) = run_sync(Some(id), false) {
            eprintln!("yasgm: post-exit sync failed: {err:#}");
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

// ---- watch (daemon) --------------------------------------------------------

/// Existing on-disk save roots to watch, per eligible game.
/// Which game the daemon considers running, with an exit grace period: a
/// game only counts as exited after being gone for `EXIT_GRACE`. Proton
/// runs a short setup step under Steam's launcher, exits it, and starts the
/// real game seconds later (seen on SteamOS); without the grace the daemon
/// synced mid-launch.
///
/// Steam's own start/stop notifications (forwarded by the Decky plugin over
/// the control socket) take precedence: a hinted game counts as running
/// until its stop hint, which exits it immediately; afterwards the detector
/// seeing that game's processes wind down is ignored for a while.
struct GameSession {
    active: Option<u64>,
    gone_since: Option<std::time::Instant>,
    hinted: Option<u64>,
    stopped: Option<(u64, std::time::Instant)>,
}

impl GameSession {
    const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long after a stop hint the stopped game's lingering processes
    /// (Proton teardown, the launcher) are ignored.
    const STOP_QUIET: std::time::Duration = std::time::Duration::from_secs(30);

    fn new(running: Option<u64>) -> GameSession {
        GameSession { active: running, gone_since: None, hinted: None, stopped: None }
    }

    /// Steam says `app_id` started. Returns a different game that was
    /// running before, which is therefore done.
    fn hint_started(&mut self, app_id: u64) -> Option<u64> {
        let previous = self.active.filter(|a| *a != app_id);
        self.hinted = Some(app_id);
        self.active = Some(app_id);
        self.gone_since = None;
        self.stopped = None;
        previous
    }

    /// Steam says `app_id` stopped: it exits now, if it was the running game.
    fn hint_stopped(&mut self, app_id: u64, now: std::time::Instant) -> Option<u64> {
        self.stopped = Some((app_id, now));
        if self.hinted == Some(app_id) {
            self.hinted = None;
        }
        if self.active != Some(app_id) {
            return None;
        }
        self.active = None;
        self.gone_since = None;
        Some(app_id)
    }

    /// Feeds one detector poll (`seen`); returns the game that just exited.
    fn update(&mut self, seen: Option<u64>, now: std::time::Instant) -> Option<u64> {
        if let Some(hinted) = self.hinted {
            self.active = Some(hinted);
            return None;
        }
        let quiet = self.stopped.is_some_and(|(id, at)| seen == Some(id) && now.duration_since(at) < Self::STOP_QUIET);
        let seen = if quiet { None } else { seen };
        let exited = match self.active {
            Some(game) if seen == Some(game) => {
                self.gone_since = None;
                None
            }
            // Another game took over: the previous one is done for sure.
            Some(game) if seen.is_some() => Some(game),
            Some(game) => {
                let since = *self.gone_since.get_or_insert(now);
                (now.duration_since(since) >= Self::EXIT_GRACE).then_some(game)
            }
            None => None,
        };
        if exited.is_some() || self.active.is_none() {
            self.active = seen;
            self.gone_since = None;
        }
        exited
    }
}

/// The daemon's recent sync events, numbered, for `watch send events
/// <since>` (the Decky plugin polls it for toasts). Keeps the last 100.
#[derive(Default)]
struct EventLog {
    seq: u64,
    events: std::collections::VecDeque<(u64, u64, SyncEvent)>,
}

impl EventLog {
    const CAPACITY: usize = 100;

    fn record(&mut self, event: SyncEvent) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.seq += 1;
        self.events.push_back((self.seq, at, event));
        while self.events.len() > Self::CAPACITY {
            self.events.pop_front();
        }
    }

    /// `run_sync` for the daemon: its events (or its failure) are logged.
    fn sync(&mut self, only: Option<u64>) -> Result<()> {
        // Disconnected on purpose: nothing to sync, and nothing to report.
        if matches!(config::Config::load().provider, config::ProviderConfig::None) {
            return Ok(());
        }
        match run_sync(only, false) {
            Ok(events) => {
                events.into_iter().for_each(|e| self.record(e));
                Ok(())
            }
            Err(err) => {
                self.record(SyncEvent { app_id: only, game: None, kind: "error", detail: format!("{err:#}") });
                Err(err)
            }
        }
    }

    /// `{"seq": latest, "events": [...]}` with the events after `since`;
    /// without `since`, just the latest number (a client's starting point).
    fn since(&self, since: Option<u64>) -> serde_json::Value {
        let since = since.unwrap_or(self.seq);
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|(seq, ..)| *seq > since)
            .map(|(seq, at, e)| {
                serde_json::json!({
                    "seq": seq, "at": at, "app_id": e.app_id, "game": e.game, "kind": e.kind, "detail": e.detail,
                })
            })
            .collect();
        serde_json::json!({"seq": self.seq, "events": events})
    }
}

/// One save location the daemon watches.
#[derive(Debug, Clone, PartialEq)]
struct WatchRoot {
    app_id: u64,
    /// An existing save directory, or a save file.
    root: PathBuf,
    is_dir: bool,
    /// The rule's resolved path pattern, so a new file in a file root's
    /// folder (`Game3.dat` for `Game*.dat`) counts too.
    pattern: String,
}

impl WatchRoot {
    /// What to register with the OS watcher: a directory recursively, plus
    /// its parent non-recursively to see the directory itself being
    /// removed/recreated; for a file only its folder, non-recursively. Never
    /// a whole parent tree — `~/.local/share` recursively would pull in every
    /// app's data and, through Proton prefixes' `dosdevices/z:` link, the
    /// entire filesystem.
    fn watches(&self) -> Vec<(PathBuf, notify::RecursiveMode)> {
        let parent = self.root.parent().map(Path::to_path_buf);
        let mut out = Vec::new();
        if self.is_dir {
            out.push((self.root.clone(), notify::RecursiveMode::Recursive));
        }
        if let Some(parent) = parent {
            out.push((parent, notify::RecursiveMode::NonRecursive));
        }
        out
    }

    /// Does a change at `path` concern this save location?
    fn covers(&self, path: &Path) -> bool {
        if self.is_dir {
            return path.starts_with(&self.root);
        }
        path == self.root || glob::Pattern::new(&self.pattern).is_ok_and(|p| p.matches_path(path))
    }
}

fn watch_roots(ctx: &Ctx, cfg: &config::Config) -> Vec<WatchRoot> {
    let mut roots = Vec::new();
    for game in &ctx.games {
        if cfg.game(game.app_id).mode == ModeOverride::Off {
            continue;
        }
        let Some(merged) = ctx.merged_game(game.app_id) else { continue };
        for (template, rule) in &merged.files {
            if !rule.is_save() {
                continue;
            }
            let Some(res) = resolve::resolve_rule(template, rule, ctx.os, game) else { continue };
            let Some(pattern) = res.path else { continue };
            for root in resolve::existing_matches(&pattern) {
                let entry = WatchRoot { app_id: game.app_id, is_dir: root.is_dir(), root, pattern: pattern.clone() };
                if !roots.contains(&entry) {
                    roots.push(entry);
                }
            }
        }
    }
    roots
}

/// Registers `root`'s OS watches not already registered (a path watched
/// recursively needs no extra non-recursive watch).
fn register_watches(
    watcher: &mut impl notify::Watcher,
    registered: &mut HashMap<PathBuf, notify::RecursiveMode>,
    root: &WatchRoot,
) {
    for (path, mode) in root.watches() {
        match registered.get(&path) {
            Some(notify::RecursiveMode::Recursive) => continue,
            Some(_) if mode == notify::RecursiveMode::NonRecursive => continue,
            _ => {}
        }
        match watcher.watch(&path, mode) {
            Ok(()) => {
                registered.insert(path, mode);
            }
            Err(err) => eprintln!("cannot watch {}: {err}", path.display()),
        }
    }
}

/// Control messages into the watch loop, from the tray menu or the control
/// socket (`control.rs`, e.g. the Decky plugin).
pub enum WatchCommand {
    /// Sync one game, or all.
    Sync(Option<u64>),
    TogglePause,
    SetPaused(bool),
    /// Steam reported a game start / stop (see `GameSession` hints). A
    /// start with a reply channel syncs the game first and replies when
    /// done — the Decky plugin keeps the game paused until then.
    GameStarted(u64, Option<std::sync::mpsc::Sender<String>>),
    GameStopped(u64),
    /// Reply with `EventLog::since` as JSON.
    Events(Option<u64>, std::sync::mpsc::Sender<String>),
    /// Reply with the daemon's state as one JSON line.
    Status(std::sync::mpsc::Sender<String>),
    Quit,
}

/// Answers one control-socket request by forwarding it to the watch loop.
fn remote_command(tx: &std::sync::mpsc::Sender<WatchCommand>, request: control::Request) -> String {
    use control::Request;
    // Requests answered by the loop itself; it checks commands at least
    // every 2 s, but a sync in progress holds it up.
    let ask = |make: &dyn Fn(std::sync::mpsc::Sender<String>) -> WatchCommand, wait_secs: u64| -> String {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx.send(make(reply_tx)).is_err() {
            return "error: watch loop has stopped".into();
        }
        reply_rx
            .recv_timeout(std::time::Duration::from_secs(wait_secs))
            .unwrap_or_else(|_| "error: daemon is busy (syncing); try again".into())
    };
    let command = match request {
        // Waits for the pre-launch sync (see WatchCommand::GameStarted).
        Request::GameStarted(id) => return ask(&|reply| WatchCommand::GameStarted(id, Some(reply)), 150),
        Request::Events(since) => return ask(&|reply| WatchCommand::Events(since, reply), 8),
        Request::GameStopped(id) => WatchCommand::GameStopped(id),
        Request::Sync(target) => WatchCommand::Sync(target),
        Request::Pause => WatchCommand::SetPaused(true),
        Request::Resume => WatchCommand::SetPaused(false),
        Request::Quit => WatchCommand::Quit,
        Request::Status => return ask(&WatchCommand::Status, 8),
    };
    match tx.send(command) {
        Ok(()) => "ok".into(),
        Err(_) => "error: watch loop has stopped".into(),
    }
}

/// Daemon entry: headless by default, `--tray` adds a status icon with
/// Sync now / Pause / Quit (Windows + macOS; Linux tray pending).
/// `watch status [--json]` reports whether a daemon is running instead;
/// `watch stop` / `watch start` stop it or launch it detached (used by the
/// GUI around self-updates, D20, and to restore it afterwards).
fn watch_cmd(args: &[String]) -> Result<()> {
    match positionals(args).first().map(String::as_str) {
        Some("stop") => return watch_stop(args),
        Some("start") => return watch_start(args),
        Some("send") => {
            let request = positionals(args)[1..].join(" ");
            control::parse(&request).context(control::USAGE)?;
            let reply = control::send(&request)?;
            println!("{reply}");
            if reply.starts_with("error") {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    if positionals(args).first().map(String::as_str) == Some("status") {
        let running = lock::daemon_status()?;
        if flag(args, "--json") {
            println!(
                "{}",
                serde_json::json!({"running": running.is_some(), "pid": running.flatten()})
            );
        } else {
            match running {
                Some(Some(pid)) => println!("watch daemon: running (pid {pid})"),
                Some(None) => println!("watch daemon: running"),
                None => println!("watch daemon: not running"),
            }
        }
        return Ok(());
    }
    // Held until this function returns, i.e. for the daemon's whole life
    // (tray mode runs its event loop inside tray_main).
    let _lock = match lock::acquire_daemon() {
        Ok(lock) => lock,
        Err(err) if err.is::<lock::AlreadyRunning>() => {
            eprintln!("yasgm: {err} — not starting a second one");
            std::process::exit(3);
        }
        Err(err) => return Err(err),
    };
    let settle = std::time::Duration::from_secs(
        flag_value(args, "--settle").and_then(|s| s.parse().ok()).unwrap_or(10),
    );
    if flag(args, "--tray") {
        #[cfg(any(windows, target_os = "macos"))]
        return tray::tray_main(settle);
        #[cfg(not(any(windows, target_os = "macos")))]
        eprintln!("tray is not supported on this OS yet; running headless watch");
    }
    let (control_tx, control_rx) = std::sync::mpsc::channel();
    watch_loop(settle, (control_tx, control_rx), None)
}

/// Stops the running daemon (if any) and waits for its lock to free: a
/// clean `quit` over the control socket first (it finishes a sync in
/// progress), then terminate. Terminating is safe too — every local state
/// write is atomic (fsutil) and cloud versions are written before their
/// index entry — just not as polite.
fn watch_stop(args: &[String]) -> Result<()> {
    let was_running = lock::daemon_status()?;
    let quit_cleanly = was_running.is_some()
        && control::send("quit").is_ok_and(|reply| reply == "ok")
        && lock::wait_until_stopped(std::time::Duration::from_secs(30))?;
    if let (Some(pid), false) = (was_running, quit_cleanly) {
        let pid = pid.context("daemon is running but its pid is unknown (watch.pid missing)")?;
        terminate(pid, false);
        if !lock::wait_until_stopped(std::time::Duration::from_secs(10))? {
            terminate(pid, true);
            anyhow::ensure!(
                lock::wait_until_stopped(std::time::Duration::from_secs(5))?,
                "watch daemon (pid {pid}) did not stop"
            );
        }
    }
    if flag(args, "--json") {
        println!("{}", serde_json::json!({"was_running": was_running.is_some()}));
    } else if was_running.is_some() {
        println!("watch daemon stopped");
    } else {
        println!("watch daemon was not running");
    }
    Ok(())
}

fn terminate(pid: u32, force: bool) {
    #[cfg(unix)]
    let result = std::process::Command::new("kill")
        .args([if force { "-KILL" } else { "-TERM" }, &pid.to_string()])
        .output();
    #[cfg(windows)]
    let result = {
        let _ = force; // no graceful signal for a windowless process on Windows
        std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output()
    };
    if let Err(err) = result {
        eprintln!("could not signal pid {pid}: {err}");
    }
}

/// Launches `watch` (with the tray where supported) as a detached process
/// and waits until it holds the daemon lock. No-op if one already runs.
fn watch_start(args: &[String]) -> Result<()> {
    let pid = match lock::daemon_status()? {
        Some(pid) => pid,
        None => {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command.arg("watch");
            #[cfg(any(windows, target_os = "macos"))]
            command.arg("--tray");
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                // Own process group: not torn down with whoever launched us.
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
            }
            command.spawn().context("launching watch daemon")?;
            lock::wait_until_running(std::time::Duration::from_secs(10))?
                .context("watch daemon did not start within 10 s")?
        }
    };
    if flag(args, "--json") {
        println!("{}", serde_json::json!({"running": true, "pid": pid}));
    } else {
        println!("watch daemon running{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
    }
    Ok(())
}

/// FS-watch save dirs, debounce, and sync when a game's saves settle and the
/// game isn't running (or as soon as it exits). The launch wrapper
/// (`yasgm run`) remains the strongest guarantee; this covers unwrapped
/// launches.
fn watch_loop(
    settle: std::time::Duration,
    (control_tx, control_rx): (std::sync::mpsc::Sender<WatchCommand>, std::sync::mpsc::Receiver<WatchCommand>),
    status: Option<std::sync::mpsc::Sender<String>>,
) -> Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const RESCAN_EVERY: Duration = Duration::from_secs(300);
    let report = |line: &str| {
        if let Some(tx) = &status {
            let _ = tx.send(line.to_owned());
        }
    };

    let ctx = load_ctx()?;
    let cfg = config::Config::load();

    if let Err(err) = control::serve(move |request| remote_command(&control_tx, request)) {
        eprintln!("control socket unavailable (watch send won't reach this daemon): {err:#}");
    }

    let mut log = EventLog::default();
    println!("initial sync pass…");
    report("initial sync…");
    if let Err(err) = log.sync(None) {
        eprintln!("initial sync failed (will keep watching): {err:#}");
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })?;
    let mut roots = watch_roots(&ctx, &cfg);
    let mut registered = HashMap::new();
    for root in &roots {
        register_watches(&mut watcher, &mut registered, root);
    }
    let n_games = roots.iter().map(|r| r.app_id).collect::<std::collections::HashSet<_>>().len();
    println!(
        "watching {} paths across {n_games} games (settle {}s); Ctrl-C to stop",
        roots.len(),
        settle.as_secs()
    );
    report(&format!("watching {n_games} games"));

    let mut dirty: HashMap<u64, Instant> = HashMap::new();
    let mut detector = running::RunningDetector::new();
    let mut session = GameSession::new(detector.poll(&ctx.root, &ctx.games));
    let mut last_rescan = Instant::now();
    let mut paused = false;

    let handle_event = |event: notify::Result<notify::Event>,
                            dirty: &mut HashMap<u64, Instant>,
                            roots: &[WatchRoot]| {
        let Ok(event) = event else { return };
        if event.kind.is_access() {
            return; // reads (e.g. our own snapshot) aren't changes
        }
        for path in &event.paths {
            for root in roots.iter().filter(|r| r.covers(path)) {
                if dirty.insert(root.app_id, Instant::now()).is_none() {
                    println!("change detected for app {} ({})", root.app_id, path.display());
                }
            }
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(event) => handle_event(event, &mut dirty, &roots),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(event) = rx.try_recv() {
            handle_event(event, &mut dirty, &roots);
        }

        let mut exits: Vec<u64> = Vec::new();
        while let Ok(command) = control_rx.try_recv() {
            match command {
                WatchCommand::Quit => {
                    report("stopped");
                    control::remove_socket();
                    return Ok(());
                }
                WatchCommand::TogglePause | WatchCommand::SetPaused(_) => {
                    paused = match command {
                        WatchCommand::SetPaused(value) => value,
                        _ => !paused,
                    };
                    println!("{}", if paused { "paused" } else { "resumed" });
                    report(if paused { "paused" } else { "resumed — watching" });
                }
                WatchCommand::Sync(target) => {
                    report("syncing…");
                    match log.sync(target) {
                        Ok(()) => report("synced (manual)"),
                        Err(err) => {
                            eprintln!("manual sync failed: {err:#}");
                            report("sync failed — see log");
                        }
                    }
                    match target {
                        Some(app_id) => {
                            dirty.remove(&app_id);
                        }
                        None => dirty.clear(),
                    }
                }
                // Steam reports non-Steam shortcuts and tools too; only
                // installed Steam games matter.
                WatchCommand::GameStarted(app_id, reply) => {
                    if ctx.games.iter().any(|g| g.app_id == app_id) {
                        // The plugin holds the game paused until we reply:
                        // pull newer cloud saves before it reads them.
                        if !paused {
                            println!("Steam reports app {app_id} starting — syncing it first");
                            if let Err(err) = log.sync(Some(app_id)) {
                                eprintln!("pre-launch sync of {app_id} failed: {err:#}");
                            }
                            dirty.remove(&app_id);
                        } else {
                            println!("Steam reports app {app_id} started");
                        }
                        exits.extend(session.hint_started(app_id));
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send("ok".into());
                    }
                }
                WatchCommand::GameStopped(app_id) if !ctx.games.iter().any(|g| g.app_id == app_id) => {}
                WatchCommand::GameStopped(app_id) => {
                    println!("Steam reports app {app_id} stopped");
                    exits.extend(session.hint_stopped(app_id, Instant::now()));
                }
                WatchCommand::Events(since, reply) => {
                    let _ = reply.send(log.since(since).to_string());
                }
                WatchCommand::Status(reply) => {
                    let mut pending: Vec<u64> = dirty.keys().copied().collect();
                    pending.sort();
                    let _ = reply.send(
                        serde_json::json!({
                            "paused": paused,
                            "running_game": session.active,
                            "pending_sync": pending,
                            "watching_games": n_games,
                        })
                        .to_string(),
                    );
                }
            }
        }

        exits.extend(session.update(detector.poll(&ctx.root, &ctx.games), Instant::now()));
        // Still "running" during the exit grace period, so settled saves wait too.
        let running_now = session.active;
        for prev in exits {
            if !paused {
                println!("app {prev} exited — syncing");
                report("game exited — syncing…");
                match log.sync(Some(prev)) {
                    Ok(()) => report("synced after game exit"),
                    Err(err) => {
                        eprintln!("sync after exit of {prev} failed: {err:#}");
                        report("sync failed — see log");
                    }
                }
                dirty.remove(&prev);
            }
        }

        if !paused {
            let settled: Vec<u64> = dirty
                .iter()
                .filter(|(app_id, since)| {
                    since.elapsed() >= settle && running_now != Some(**app_id)
                })
                .map(|(app_id, _)| *app_id)
                .collect();
            for app_id in settled {
                println!("saves settled for app {app_id} — syncing");
                report("saves changed — syncing…");
                match log.sync(Some(app_id)) {
                    Ok(()) => report("synced"),
                    Err(err) => {
                        eprintln!("sync of {app_id} failed: {err:#}");
                        report("sync failed — see log");
                    }
                }
                dirty.remove(&app_id);
            }
        }

        // Pick up save dirs that didn't exist at startup (first-ever save).
        if last_rescan.elapsed() >= RESCAN_EVERY {
            last_rescan = Instant::now();
            let fresh = watch_roots(&ctx, &cfg);
            for root in fresh.iter().filter(|r| !roots.contains(r)) {
                register_watches(&mut watcher, &mut registered, root);
                println!("now watching {} (app {})", root.root.display(), root.app_id);
            }
            roots = fresh;
        }
    }
    Ok(())
}

/// Opt-in autostart at login (D11).
fn autostart_cmd(args: &[String]) -> Result<()> {
    match positionals(args).first().map(String::as_str) {
        Some("on") => println!("{}", autostart::enable()?),
        Some("off") => println!("{}", autostart::disable()?),
        None | Some("status") => {
            let detail = autostart::status()?;
            if flag(args, "--json") {
                println!(
                    "{}",
                    serde_json::json!({"enabled": detail.is_some(), "detail": detail})
                );
            } else {
                match detail {
                    Some(detail) => println!("autostart: on ({detail})"),
                    None => println!("autostart: off"),
                }
            }
        }
        Some(other) => anyhow::bail!("usage: yasgm autostart [on|off|status] (got {other:?})"),
    }
    Ok(())
}

/// Debug: show what the running-game detector sees right now.
fn running_cmd() -> Result<()> {
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
        // Best-effort: catch up any pending (offline) backups before doing
        // anything else — see the matching comment in sync::sync_game.
        if !dry_run {
            let _ = store.flush_pending(game.app_id, keep);
        }
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
                            let verb = if version.pending { "backed up locally (pending — offline)" } else { "uploaded" };
                            println!(
                                "{}: {verb} version {} ({} files, {})",
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
    let json = flag(args, "--json");
    let ctx = load_ctx()?;
    let store = ctx.cloud_store()?;
    let mut json_out = Vec::new();

    for game in ctx.games.iter().filter(|g| only.is_none_or(|id| id == g.app_id)) {
        let Some(index) = store.load_index(game.app_id)? else {
            if !json {
                println!("{} ({}): no cloud versions", game.name, game.app_id);
            }
            continue;
        };
        if !json {
            println!("{} ({}):", game.name, game.app_id);
        }
        let head_id = Store::head(&index).map(|h| h.id.clone());
        let mut versions = index.versions.clone();
        versions.sort_by(|a, b| b.created.cmp(&a.created));
        for v in versions {
            let active = Some(&v.id) == head_id.as_ref();
            if json {
                json_out.push(serde_json::json!({
                    "app_id": game.app_id,
                    "game": game.name,
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
    let files = ctx.merged_game(app_id).map(|m| m.files).unwrap_or_default();
    if let Some(from) = Os::from_name(&version.os).filter(|from| *from != ctx.os) {
        if resolve::cross_os_gaps(&files).contains(&(from, ctx.os)) {
            eprintln!(
                "warning: {} was made on {}, and this game's save locations on {} and {} don't match — \
                 its files will likely be skipped",
                version.id,
                from.label(),
                from.label(),
                ctx.os.label()
            );
        }
    }
    let written = snapshot::extract(&bytes, &version.mounts, &files, ctx.os, Os::from_name(&version.os), game, dry_run)?;
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
fn icons_cmd(args: &[String]) -> Result<()> {
    let root = steam::find_steam_root();
    let pos = positionals(args);
    let app_ids: Vec<u64> = if pos.is_empty() {
        let root = root.as_ref().context("Steam installation not found")?;
        steam::installed_games(&steam::libraries(root)?)?.iter().map(|g| g.app_id).collect()
    } else {
        pos.iter()
            .map(|p| p.parse().context("usage: yasgm icons [appid...] [--offline] [--json] [--embed]"))
            .collect::<Result<_>>()?
    };
    let dir = icons::cache_dir()?;
    if !flag(args, "--offline") {
        let missing: Vec<u64> =
            app_ids.iter().copied().filter(|&id| icons::resolve(root.as_deref(), &dir, id).is_none()).collect();
        if !missing.is_empty() {
            if let Err(err) = icons::fetch(&dir, &missing) {
                eprintln!("icons: download skipped: {err:#}");
            }
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
    let store = Store::new(
        Box::new(onedrive::OneDriveProvider::new(access_token.clone())),
        "selftest".to_owned(),
    )?;

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
    let written = snapshot::extract(&bytes, &head.mounts, &files, os, Some(os), &game, false)?;
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

#[cfg(test)]
mod tests {
    use super::{parse_run_args, EventLog, GameSession, SyncEvent, WatchRoot};
    use std::path::{Path, PathBuf};

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
    fn proton_setup_gap_is_not_an_exit() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut session = GameSession::new(None);
        // Proton setup step runs, exits, real game starts 3 s later.
        assert_eq!(session.update(Some(2140020), at(0)), None);
        assert_eq!(session.update(None, at(1)), None);
        assert_eq!(session.active, Some(2140020), "still running during the grace period");
        assert_eq!(session.update(None, at(3)), None);
        assert_eq!(session.update(Some(2140020), at(4)), None);
        // Real exit: reported once, after the grace period.
        assert_eq!(session.update(None, at(60)), None);
        assert_eq!(session.update(None, at(65)), None);
        assert_eq!(session.update(None, at(70)), Some(2140020));
        assert_eq!(session.active, None);
        assert_eq!(session.update(None, at(80)), None, "no second exit");
    }

    #[test]
    fn event_log_returns_only_newer_events_and_caps_its_size() {
        let event = |kind| SyncEvent { app_id: Some(1), game: Some("G".into()), kind, detail: String::new() };
        let mut log = EventLog::default();
        assert_eq!(log.since(None), serde_json::json!({"seq": 0, "events": []}));
        log.record(event("uploaded"));
        log.record(event("conflict"));
        let start = log.since(None);
        assert_eq!(start["seq"], 2);
        assert!(start["events"].as_array().unwrap().is_empty(), "no backlog for a new client");
        let newer = log.since(Some(1));
        assert_eq!(newer["events"].as_array().unwrap().len(), 1);
        assert_eq!(newer["events"][0]["kind"], "conflict");
        for _ in 0..150 {
            log.record(event("uploaded"));
        }
        assert_eq!(log.events.len(), EventLog::CAPACITY);
        assert_eq!(log.since(Some(0))["seq"], 152);
    }

    #[test]
    fn steam_hints_override_detection() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut session = GameSession::new(None);
        assert_eq!(session.hint_started(42), None);
        // Detection missing the game doesn't end a hinted session.
        assert_eq!(session.update(None, at(30)), None);
        assert_eq!(session.active, Some(42));
        // Stop hint exits at once, no grace period.
        assert_eq!(session.hint_stopped(42, at(31)), Some(42));
        assert_eq!(session.active, None);
        // The launcher lingering while Proton shuts down isn't a new session.
        assert_eq!(session.update(Some(42), at(33)), None);
        assert_eq!(session.active, None);
        assert_eq!(session.update(None, at(50)), None, "no second exit");
        // A stop hint for a game that isn't running does nothing.
        assert_eq!(session.hint_stopped(7, at(60)), None);
    }

    #[test]
    fn switching_games_exits_the_previous_one_immediately() {
        use std::time::Instant;
        let now = Instant::now();
        let mut session = GameSession::new(Some(1));
        assert_eq!(session.update(Some(2), now), Some(1));
        assert_eq!(session.active, Some(2));
    }

    #[test]
    fn watch_root_ignores_siblings_of_a_save_directory() {
        let root = WatchRoot {
            app_id: 736260,
            root: PathBuf::from("/home/u/.local/share/Baba_Is_You"),
            is_dir: true,
            pattern: "/home/u/.local/share/Baba_Is_You".into(),
        };
        assert!(root.covers(Path::new("/home/u/.local/share/Baba_Is_You/ba.ba")));
        assert!(root.covers(Path::new("/home/u/.local/share/Baba_Is_You")));
        assert!(!root.covers(Path::new("/home/u/.local/share/sddm/state.conf")));
        assert!(!root.covers(Path::new("/home/u/.local/share/Baba_Is_You_old/x")));
        // Never the parent tree recursively.
        let watches = root.watches();
        assert!(watches.contains(&(PathBuf::from("/home/u/.local/share/Baba_Is_You"), notify::RecursiveMode::Recursive)));
        assert!(watches.contains(&(PathBuf::from("/home/u/.local/share"), notify::RecursiveMode::NonRecursive)));
        assert!(!watches.contains(&(PathBuf::from("/home/u/.local/share"), notify::RecursiveMode::Recursive)));
    }

    #[test]
    fn watch_root_for_a_file_pattern_catches_new_matching_files_only() {
        let root = WatchRoot {
            app_id: 331920,
            root: PathBuf::from("/games/ATD/Savegame/Game1.dat"),
            is_dir: false,
            pattern: "/games/ATD/Savegame/Game*.dat".into(),
        };
        assert!(root.covers(Path::new("/games/ATD/Savegame/Game1.dat")));
        assert!(root.covers(Path::new("/games/ATD/Savegame/Game7.dat")), "a new save matching the pattern");
        assert!(!root.covers(Path::new("/games/ATD/Savegame/Options.cfg")));
        assert_eq!(root.watches(), vec![(PathBuf::from("/games/ATD/Savegame"), notify::RecursiveMode::NonRecursive)]);
    }

    #[test]
    fn run_args_empty_command() {
        let (app, cmd) = parse_run_args(&s(&["run", "--app", "42"]));
        assert_eq!(app, Some(42));
        assert!(cmd.is_empty());
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("doctor") => doctor(),
        Some("auth") => auth(&args),
        Some("status") => status(&args),
        Some("games") => {
            ensure!(positionals(&args).is_empty(), "usage: yasgm games [--json]");
            config_cmd(&args)
        }
        Some("sync") => sync_cmd(&args),
        Some("run") => run_cmd(&args),
        Some("watch") => watch_cmd(&args),
        Some("autostart") => autostart_cmd(&args),
        Some("provider") => provider_cmd(&args),
        Some("paths") => paths_cmd(&args),
        Some("backup-location") => backup_location_cmd(&args),
        Some("pending-location") => pending_location_cmd(&args),
        Some("running") => running_cmd(),
        Some("backup") => backup_cmd(&args),
        Some("versions") => versions_cmd(&args),
        Some("restore") => restore_cmd(&args),
        Some("config") => config_cmd(&args),
        Some("icons") => icons_cmd(&args),
        Some("gui") => gui::open(),
        Some("manifest-gaps") => manifest_gaps_cmd(&args),
        Some("pin") => pin_cmd(&args, true),
        Some("unpin") => pin_cmd(&args, false),
        Some("rm") => rm_cmd(&args),
        Some("selftest") => selftest(),
        Some(other) => {
            eprintln!(
                "unknown command {other:?}\navailable:\n  \
                 doctor\n  auth [--device [--json]]\n  status [--json]\n  games [--json]\n  \
                 sync [appid] [--dry-run]\n  run [--app <appid>] -- <game command...>\n  \
                 watch [--settle <secs>] [--tray] | watch status|start|stop [--json] | watch send <request>\n  autostart [on|off|status] [--json]\n  \
                 provider [onedrive|local <path>|disconnect|status] [--json]\n  \
                 paths <appid>\n  backup-location <appid>\n  \
                 pending-location <appid>\n  \
                 backup [appid] [--dry-run]\n  \
                 versions [appid] [--json]\n  restore <appid> [--version <id>] [--dry-run]\n  \
                 config [--json | <appid> --mode auto|sync|backup|off --keep N|default | --clear]\n  \
                 icons [appid...] [--offline] [--json] [--embed]\n  gui\n  \
                 pin <appid> <version-id>\n  unpin <appid> <version-id>\n  \
                 rm <appid> <version-id>"
            );
            std::process::exit(2);
        }
    }
}
