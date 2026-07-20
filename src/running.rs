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
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        for process in self.system.processes().values() {
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
