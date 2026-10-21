//! Detect which Steam game is currently running. Layered (DESIGN.md):
//!
//! 1. Steam's own state — Windows: `HKCU\Software\Valve\Steam\RunningAppID`;
//!    Linux: `RunningAppID` in `~/.steam/registry.vdf` (written per docs,
//!    not yet validated on hardware). macOS: registry.vdf carries no
//!    RunningAppID at all (verified 2026-07-20), so this layer never fires
//!    there.
//! 2. Process scan fallback (all OSes): any running process whose executable
//!    lives under a game's `steamapps/common/<installdir>` — also catches
//!    games launched outside Steam and the titles that don't update the
//!    registry key reliably.

use std::path::Path;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::steam::InstalledGame;

#[cfg(windows)]
fn steam_reported_app_id(_steam_root: &Path) -> Option<u64> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Valve\\Steam")
        .ok()?;
    let value: u32 = key.get_value("RunningAppID").ok()?;
    (value != 0).then_some(value as u64)
}

#[cfg(not(windows))]
fn steam_reported_app_id(steam_root: &Path) -> Option<u64> {
    let mut candidates = vec![steam_root.join("registry.vdf")];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".steam/registry.vdf"));
        // Flatpak Steam keeps its own copy inside the sandbox home.
        candidates.push(home.join(".var/app/com.valvesoftware.Steam/.steam/registry.vdf"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = crate::vdf::parse(&text) else {
            continue;
        };
        let value = crate::vdf::get_obj(&doc, "registry")
            .and_then(|o| crate::vdf::get_obj(o, "hkcu"))
            .and_then(|o| crate::vdf::get_obj(o, "software"))
            .and_then(|o| crate::vdf::get_obj(o, "valve"))
            .and_then(|o| crate::vdf::get_obj(o, "steam"))
            .and_then(|o| crate::vdf::get_str(o, "runningappid"))
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(id) = value {
            return (id != 0).then_some(id);
        }
    }
    None
}

/// The app a process belongs to, from how Steam launched it. Linux Steam
/// starts every game under `reaper SteamLaunch AppId=<id> -- …`, alive for
/// the whole session, and game processes inherit `SteamAppId=<id>`. Unlike
/// the exe-path check this works when the visible executable isn't in the
/// install dir — Proton's Wine loader, or FEX emulating x86 on ARM — and
/// SteamOS doesn't keep `RunningAppID` in registry.vdf current (observed in
/// the steamac VM: never set while Stronghold or Baba Is You ran).
fn steam_launch_app_id(cmd: &[std::ffi::OsString], environ: &[std::ffi::OsString]) -> Option<u64> {
    let arg_id = |prefix: &str, list: &[std::ffi::OsString]| {
        list.iter().find_map(|a| a.to_str()?.strip_prefix(prefix)?.parse::<u64>().ok())
    };
    if cmd.iter().any(|a| a == "SteamLaunch")
        && let Some(id) = arg_id("AppId=", cmd) {
            return Some(id);
        }
    arg_id("SteamAppId=", environ).filter(|id| *id != 0)
}

pub struct RunningDetector {
    system: System,
}

pub fn debug_dump(games: &[InstalledGame]) {
    let mut detector = RunningDetector::new();
    detector.debug_processes(games);
}

impl RunningDetector {
    pub fn new() -> RunningDetector {
        RunningDetector { system: System::new() }
    }

    pub fn poll(&mut self, steam_root: &Path, games: &[InstalledGame]) -> Option<u64> {
        if let Some(app_id) = steam_reported_app_id(steam_root) {
            return Some(app_id);
        }
        self.process_scan(games)
    }

    pub fn debug_processes(&mut self, games: &[InstalledGame]) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        let (mut with_exe, mut total) = (0u32, 0u32);
        for process in self.system.processes().values() {
            total += 1;
            if let Some(exe) = process.exe() {
                with_exe += 1;
                let display = exe.to_string_lossy();
                if display.contains("steamapps") {
                    println!("  candidate: {display}");
                }
            }
        }
        println!("  processes: {total}, with exe path: {with_exe}");
        for game in games {
            println!(
                "  install dir: {}",
                game.library.join("steamapps/common").join(&game.install_dir).display()
            );
        }
    }

    fn process_scan(&mut self, games: &[InstalledGame]) -> Option<u64> {
        if games.is_empty() {
            return None;
        }
        let install_dirs: Vec<(u64, std::path::PathBuf)> = games
            .iter()
            .map(|g| (g.app_id, g.library.join("steamapps/common").join(&g.install_dir)))
            .collect();
        // cmd/environ only matter on Linux (see steam_launch_app_id); a
        // process's /proc environ never changes, so caching is fine.
        let refresh = ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet);
        let refresh = if cfg!(target_os = "linux") {
            refresh.with_cmd(UpdateKind::OnlyIfNotSet).with_environ(UpdateKind::OnlyIfNotSet)
        } else {
            refresh
        };
        self.system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        for process in self.system.processes().values() {
            if let Some(app_id) = steam_launch_app_id(process.cmd(), process.environ())
                && games.iter().any(|g| g.app_id == app_id) {
                    return Some(app_id);
                }
            let Some(exe) = process.exe() else { continue };
            for (app_id, dir) in &install_dirs {
                if exe.starts_with(dir) {
                    return Some(*app_id);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<std::ffi::OsString> {
        v.iter().map(Into::into).collect()
    }

    #[test]
    fn reads_app_id_from_steams_reaper_command_line() {
        let cmd = os(&["/home/u/.local/share/Steam/ubuntu12_32/reaper", "SteamLaunch", "AppId=2140020", "--", "/x/proton"]);
        assert_eq!(steam_launch_app_id(&cmd, &[]), Some(2140020));
    }

    #[test]
    fn reads_app_id_from_inherited_environment() {
        let env = os(&["HOME=/home/u", "SteamAppId=736260", "SteamGameId=736260"]);
        assert_eq!(steam_launch_app_id(&os(&["/usr/bin/FEXInterpreter", "Baba Is You/bin64/Chowdren"]), &env), Some(736260));
    }

    #[test]
    fn ignores_unrelated_processes_and_zero_ids() {
        assert_eq!(steam_launch_app_id(&os(&["/usr/bin/bash"]), &os(&["HOME=/home/u"])), None);
        assert_eq!(steam_launch_app_id(&os(&["steam"]), &os(&["SteamAppId=0"])), None);
        // "AppId=" without the SteamLaunch marker isn't Steam's launcher.
        assert_eq!(steam_launch_app_id(&os(&["tool", "AppId=5"]), &[]), None);
    }
}
