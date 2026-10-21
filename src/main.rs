//! YASGM — Yet another save game manager. `main` dispatches to the
//! commands (`commands/`, `daemon`); the engine is `sync` + `store` +
//! `snapshot` over a `provider`.

mod activity;
mod args;
mod autostart;
mod commands;
mod config;
mod control;
mod ctx;
mod daemon;
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
mod signature;
mod snapshot;
mod steam;
mod store;
mod sync;
mod tray;
mod vdf;

use anyhow::{Result, ensure};

use args::positionals;
use commands::games::*;
use commands::saves::*;
use commands::selftest::selftest;
use commands::setup::*;
use commands::sync::*;
use daemon::watch_cmd;

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
        Some("cloud-games") => cloud_games_cmd(&args),
        Some("move-account") => move_account_cmd(&args),
        Some("restore") => restore_cmd(&args),
        Some("config") => config_cmd(&args),
        Some("icons") => icons_cmd(&args),
        Some("gui") => gui::open(),
        Some("manifest-gaps") => manifest_gaps_cmd(&args),
        Some("pin") => pin_cmd(&args, true),
        Some("unpin") => pin_cmd(&args, false),
        Some("rm") => rm_cmd(&args),
        Some("verify-update") => verify_update_cmd(&args),
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
