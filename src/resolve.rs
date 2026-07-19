//! Resolve manifest path templates (placeholders like <winAppData>) into
//! concrete paths for the current OS, including Proton prefixes on Linux.

use std::path::{Path, PathBuf};

use crate::manifest::{Constraint, FileRule};
use crate::steam::InstalledGame;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Os {
    Windows,
    Linux,
    Mac,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Linux => "linux",
            Os::Mac => "mac",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Via {
    Native,
    Proton,
}

#[derive(Debug)]
pub struct Resolution {
    pub template: String,
    pub via: Via,
    /// Concrete path, possibly containing `*` wildcards (e.g. <storeUserId>).
    pub path: Option<String>,
    pub note: Option<&'static str>,
}

fn constraint_matches(c: &Constraint, os: Os) -> bool {
    let os_ok = match &c.os {
        None => true,
        Some(name) => name == os.name(),
    };
    let store_ok = matches!(c.store.as_deref(), None | Some("steam"));
    os_ok && store_ok
}

/// Does this rule apply on `os` (natively), per its `when` constraints?
fn applies_on(rule: &FileRule, os: Os) -> bool {
    rule.when.is_empty() || rule.when.iter().any(|c| constraint_matches(c, os))
}

/// Is this rule Windows-only (candidate for Proton resolution on Linux)?
fn windows_only(rule: &FileRule) -> bool {
    !rule.when.is_empty()
        && rule.when.iter().all(|c| c.os.as_deref() == Some("windows"))
        && rule.when.iter().any(|c| constraint_matches(c, Os::Windows))
}

fn substitute(template: &str, vars: &[(&str, String)]) -> Option<String> {
    let mut out = template.to_owned();
    for (key, value) in vars {
        out = out.replace(key, value);
    }
    // Any placeholder left unsubstituted means we can't resolve this rule.
    if out.contains('<') { None } else { Some(out) }
}

fn native_vars(os: Os, game: &InstalledGame) -> Vec<(&'static str, String)> {
    let home = dirs::home_dir().unwrap_or_default();
    let home_s = home.to_string_lossy().into_owned();
    let base = game
        .library
        .join("steamapps/common")
        .join(&game.install_dir);
    let mut vars = vec![
        ("<home>", home_s.clone()),
        ("<base>", base.to_string_lossy().into_owned()),
        ("<root>", game.library.to_string_lossy().into_owned()),
        ("<game>", game.install_dir.clone()),
        ("<storeGameId>", game.app_id.to_string()),
        ("<storeUserId>", "*".to_owned()),
        ("<osUserName>", whoami()),
    ];
    match os {
        Os::Linux => {
            let xdg_data = std::env::var("XDG_DATA_HOME")
                .unwrap_or_else(|_| format!("{home_s}/.local/share"));
            let xdg_config = std::env::var("XDG_CONFIG_HOME")
                .unwrap_or_else(|_| format!("{home_s}/.config"));
            vars.push(("<xdgData>", xdg_data));
            vars.push(("<xdgConfig>", xdg_config));
        }
        Os::Windows => {
            for (key, env) in [
                ("<winAppData>", "APPDATA"),
                ("<winLocalAppData>", "LOCALAPPDATA"),
                ("<winPublic>", "PUBLIC"),
                ("<winProgramData>", "ProgramData"),
                ("<winDir>", "windir"),
            ] {
                if let Ok(value) = std::env::var(env) {
                    vars.push((key, value));
                }
            }
            if let Ok(profile) = std::env::var("USERPROFILE") {
                vars.push(("<winLocalAppDataLow>", format!("{profile}\\AppData\\LocalLow")));
                // Known Folder API is the correct source (Documents may be
                // relocated); good enough for the spike.
                vars.push(("<winDocuments>", format!("{profile}\\Documents")));
            }
        }
        Os::Mac => {}
    }
    vars
}

/// Placeholder mapping inside a Proton (Wine) prefix on Linux/SteamOS.
fn proton_vars(game: &InstalledGame) -> Vec<(&'static str, String)> {
    let pfx = game
        .library
        .join("steamapps/compatdata")
        .join(game.app_id.to_string())
        .join("pfx");
    let user = pfx.join("drive_c/users/steamuser");
    let p = |tail: &str| -> String {
        user.join(tail).to_string_lossy().into_owned()
    };
    let base = game
        .library
        .join("steamapps/common")
        .join(&game.install_dir);
    vec![
        ("<winAppData>", p("AppData/Roaming")),
        ("<winLocalAppData>", p("AppData/Local")),
        ("<winLocalAppDataLow>", p("AppData/LocalLow")),
        ("<winDocuments>", p("Documents")),
        ("<winPublic>", pfx.join("drive_c/users/Public").to_string_lossy().into_owned()),
        ("<winProgramData>", pfx.join("drive_c/ProgramData").to_string_lossy().into_owned()),
        ("<winDir>", pfx.join("drive_c/windows").to_string_lossy().into_owned()),
        ("<home>", user.to_string_lossy().into_owned()),
        ("<base>", base.to_string_lossy().into_owned()),
        ("<root>", game.library.to_string_lossy().into_owned()),
        ("<game>", game.install_dir.clone()),
        ("<storeGameId>", game.app_id.to_string()),
        ("<storeUserId>", "*".to_owned()),
        ("<osUserName>", "steamuser".to_owned()),
    ]
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_owned())
}

/// Resolve one manifest file rule for the current OS.
pub fn resolve_rule(template: &str, rule: &FileRule, os: Os, game: &InstalledGame) -> Option<Resolution> {
    if applies_on(rule, os) {
        let path = substitute(template, &native_vars(os, game));
        let note = path.is_none().then_some("unresolvable placeholder on this OS");
        return Some(Resolution { template: template.to_owned(), via: Via::Native, path, note });
    }
    if windows_only(rule) {
        return match os {
            Os::Linux => {
                let path = substitute(template, &proton_vars(game));
                let note = path.is_none().then_some("unresolvable placeholder in Proton prefix");
                Some(Resolution { template: template.to_owned(), via: Via::Proton, path, note })
            }
            Os::Mac => Some(Resolution {
                template: template.to_owned(),
                via: Via::Native,
                path: None,
                note: Some("Windows-only; no Proton on macOS (out of scope)"),
            }),
            Os::Windows => None, // already covered by applies_on
        };
    }
    None
}

/// Resolve a bare template on this machine without consulting `when`
/// constraints — used on restore, where constraints were already honored at
/// capture time. Tries native placeholders first, then the Proton prefix on
/// Linux.
pub fn resolve_template(template: &str, os: Os, game: &InstalledGame) -> Option<String> {
    if let Some(path) = substitute(template, &native_vars(os, game)) {
        return Some(path);
    }
    if os == Os::Linux {
        return substitute(template, &proton_vars(game));
    }
    None
}

/// Expand wildcards and check what actually exists on disk.
pub fn existing_matches(path_pattern: &str) -> Vec<PathBuf> {
    if path_pattern.contains('*') {
        glob::glob(path_pattern)
            .map(|paths| paths.flatten().collect())
            .unwrap_or_default()
    } else {
        let p = Path::new(path_pattern);
        if p.exists() { vec![p.to_path_buf()] } else { Vec::new() }
    }
}

/// (file count, total bytes) under a path, capped to keep the report fast.
pub fn measure(path: &Path) -> (u64, u64) {
    const MAX_FILES: u64 = 10_000;
    fn walk(path: &Path, files: &mut u64, bytes: &mut u64) {
        if *files >= MAX_FILES {
            return;
        }
        if path.is_file() {
            *files += 1;
            if let Ok(meta) = path.metadata() {
                *bytes += meta.len();
            }
        } else if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), files, bytes);
                }
            }
        }
    }
    let (mut files, mut bytes) = (0, 0);
    walk(path, &mut files, &mut bytes);
    (files, bytes)
}
