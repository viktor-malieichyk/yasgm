//! Resolve manifest path templates (placeholders like <winAppData>) into
//! concrete paths for the current OS, including Proton prefixes on Linux.

use std::collections::HashMap;
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

/// Base directories that differ per OS. What follows one is the
/// OS-independent part of a save location, which is how a rule captured on
/// one OS is matched to its counterpart on another, e.g. `baba_is_you` for
/// both `<home>/Library/Application Support/Baba_Is_You` (mac) and
/// `<xdgData>/Baba_Is_You` (linux). Longest prefixes first.
const OS_BASE_DIRS: &[&str] = &[
    "<home>/Library/Application Support/",
    "<home>/Library/Preferences/",
    "<home>/Library/Caches/",
    "<home>/Library/Containers/",
    "<home>/.local/share/",
    "<home>/.config/",
    "<home>/Documents/",
    "<home>/Library/",
    "<home>/",
    "<xdgData>/",
    "<xdgConfig>/",
    "<winLocalAppDataLow>/",
    "<winLocalAppData>/",
    "<winAppData>/",
    "<winDocuments>/",
    "<winPublic>/",
    "<winProgramData>/",
    "<winDir>/",
];

/// OS-independent identity of a save location (see `OS_BASE_DIRS`); an
/// OS-neutral template like `<base>/save` is its own key.
fn location_key(template: &str) -> String {
    let t = template.replace('\\', "/");
    OS_BASE_DIRS
        .iter()
        .find_map(|base| t.strip_prefix(base))
        .unwrap_or(&t)
        .trim_matches('/')
        .to_lowercase()
}

fn proton_prefix(game: &InstalledGame) -> PathBuf {
    game.library.join("steamapps/compatdata").join(game.app_id.to_string()).join("pfx")
}

/// Where to restore a snapshot mount captured from `template` on this
/// machine: `(template used, concrete path, possibly with `*`)`.
///
/// The mount's own rule is used as-is when it applies natively here (same
/// OS, or OS-neutral like `<base>/save`). A rule from another OS is swapped
/// for this OS's rule for the same location (same `location_key`) —
/// otherwise a Mac `<home>/Library/Application Support/X` would be
/// recreated verbatim under a Linux home. Among counterparts, one whose
/// data (or Proton prefix) already exists wins, then native over Proton.
/// `None` when this OS has no counterpart: skip, never guess a path.
pub fn restore_target(
    template: &str,
    files: &HashMap<String, FileRule>,
    os: Os,
    game: &InstalledGame,
) -> Option<(String, String)> {
    match files.get(template) {
        Some(rule) if applies_on(rule, os) => {
            return substitute(template, &native_vars(os, game)).map(|path| (template.to_owned(), path));
        }
        // Rule no longer in the manifest: re-resolve as-is, but only when
        // nothing OS-specific is involved.
        None if location_key(template) == template.to_lowercase() => {
            return resolve_template(template, os, game).map(|path| (template.to_owned(), path));
        }
        _ => {}
    }
    let key = location_key(template);
    let mut candidates: Vec<(bool, bool, &String, String)> = files
        .iter()
        .filter(|(t, rule)| rule.is_save() && location_key(t) == key)
        .filter_map(|(t, rule)| {
            let res = resolve_rule(t, rule, os, game)?;
            let path = res.path?;
            let on_disk = match res.via {
                Via::Native => !existing_matches(&path).is_empty(),
                Via::Proton => proton_prefix(game).is_dir(),
            };
            Some((on_disk, res.via == Via::Native, t, path))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    candidates.into_iter().next().map(|(_, _, t, path)| (t.clone(), path))
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

/// Proton path mapping is pure string substitution parameterized by `Os` —
/// nothing here is gated by `cfg(target_os = ...)` — so the mapping logic
/// (per D8/D12's "needs SteamOS/Linux hardware" caveat, the one thing that
/// was previously only checked by code review, per DESIGN.md) can be
/// validated on any OS against a hand-built fake Steam library, without a
/// real Proton prefix or Linux machine. What this *doesn't* cover: real
/// Wine/Proton actually writing files there, or SteamOS-specific quirks —
/// only that our own mapping arithmetic is correct.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Constraint;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SCRATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn scratch_library(name: &str) -> PathBuf {
        let n = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yasgm-resolve-test-{name}-{n}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn fake_game(library: PathBuf, app_id: u64) -> InstalledGame {
        InstalledGame {
            app_id,
            name: "Fake Game".to_owned(),
            install_dir: "FakeGame".to_owned(),
            library,
        }
    }

    fn windows_only_rule() -> FileRule {
        FileRule {
            tags: vec!["save".to_owned()],
            when: vec![Constraint { os: Some("windows".to_owned()), store: None }],
        }
    }

    fn os_rule(os: &str) -> FileRule {
        FileRule {
            tags: vec!["save".to_owned()],
            when: vec![Constraint { os: Some(os.to_owned()), store: None }],
        }
    }

    /// Baba Is You's real rule shape: one save dir per OS.
    fn per_os_rules() -> HashMap<String, FileRule> {
        HashMap::from([
            ("<home>/Library/Application Support/Baba_Is_You".to_owned(), os_rule("mac")),
            ("<xdgData>/Baba_Is_You".to_owned(), os_rule("linux")),
            ("<winAppData>/Baba_Is_You".to_owned(), os_rule("windows")),
        ])
    }

    #[test]
    fn restore_maps_mac_location_to_linux_counterpart() {
        let game = fake_game(scratch_library("restore-mac-linux"), 736260);
        let (template, path) =
            restore_target("<home>/Library/Application Support/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
                .expect("linux has a counterpart");
        assert_eq!(template, "<xdgData>/Baba_Is_You");
        assert!(!path.contains("Library"), "mac path leaked onto linux: {path}");
        assert!(path.ends_with("/Baba_Is_You"), "{path}");
    }

    #[test]
    fn restore_maps_windows_location_to_mac_counterpart() {
        let game = fake_game(scratch_library("restore-win-mac"), 736260);
        let (template, _) = restore_target("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Mac, &game)
            .expect("mac has a counterpart");
        assert_eq!(template, "<home>/Library/Application Support/Baba_Is_You");
    }

    #[test]
    fn restore_prefers_native_linux_over_proton_without_a_prefix() {
        // A Windows-captured version restored on Linux where the game runs
        // natively (no compatdata prefix): native location, not the prefix.
        let game = fake_game(scratch_library("restore-win-linux"), 736260);
        let (template, path) = restore_target("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
            .expect("linux has a counterpart");
        assert_eq!(template, "<xdgData>/Baba_Is_You");
        assert!(!path.contains("compatdata"), "{path}");
    }

    #[test]
    fn restore_uses_proton_prefix_when_game_runs_under_proton() {
        let library = scratch_library("restore-proton");
        let game = fake_game(library.clone(), 736260);
        std::fs::create_dir_all(proton_prefix(&game)).unwrap();
        let (template, path) = restore_target("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
            .expect("prefix exists");
        assert_eq!(template, "<winAppData>/Baba_Is_You");
        assert!(path.contains("compatdata/736260/pfx"), "{path}");
        std::fs::remove_dir_all(&library).unwrap();
    }

    #[test]
    fn restore_skips_location_without_counterpart_and_keeps_neutral_ones() {
        let game = fake_game(scratch_library("restore-skip"), 1);
        let files = HashMap::from([
            ("<home>/Library/Application Support/MacOnly".to_owned(), os_rule("mac")),
            ("<base>/save".to_owned(), FileRule { tags: vec!["save".to_owned()], when: Vec::new() }),
        ]);
        assert!(restore_target("<home>/Library/Application Support/MacOnly", &files, Os::Linux, &game).is_none());
        let (template, _) = restore_target("<base>/save", &files, Os::Linux, &game).expect("neutral rule applies");
        assert_eq!(template, "<base>/save");
    }

    #[test]
    fn proton_maps_windows_placeholder_into_compatdata_prefix() {
        let library = scratch_library("compatdata");
        let game = fake_game(library.clone(), 999_001);
        let save_dir = library
            .join("steamapps/compatdata/999001/pfx/drive_c/users/steamuser/AppData/Roaming/SomeGame");
        std::fs::create_dir_all(&save_dir).unwrap();
        std::fs::write(save_dir.join("save.dat"), b"fake save").unwrap();

        let res = resolve_rule("<winAppData>/SomeGame", &windows_only_rule(), Os::Linux, &game)
            .expect("windows-only rule should resolve via Proton on Linux");
        assert_eq!(res.via, Via::Proton);
        let path = res.path.expect("all placeholders should have substituted");
        assert_eq!(PathBuf::from(&path), save_dir, "must land inside the compatdata pfx, not a native path");

        let matches = existing_matches(&path);
        assert_eq!(matches, vec![save_dir], "the real directory we created must be found");

        std::fs::remove_dir_all(&library).ok();
    }

    #[test]
    fn proton_resolves_embedded_wildcard_and_finds_real_directory() {
        // Mirrors DOS2's real manifest rule shape: <storeUserId> embedded
        // inline rather than filling a whole path segment on its own.
        let library = scratch_library("wildcard");
        let game = fake_game(library.clone(), 999_002);
        let profile_dir = library.join(
            "steamapps/compatdata/999002/pfx/drive_c/users/steamuser/Documents/Larian Studios/PlayerProfiles/Slot42Data/Savegames",
        );
        std::fs::create_dir_all(&profile_dir).unwrap();

        let template = "<winDocuments>/Larian Studios/PlayerProfiles/Slot<storeUserId>Data/Savegames";
        let res = resolve_rule(template, &windows_only_rule(), Os::Linux, &game)
            .expect("should resolve via Proton");
        assert_eq!(res.via, Via::Proton);
        let path = res.path.expect("template should substitute (wildcard stays as *)");
        assert!(path.contains('*'), "unresolved <storeUserId> should become a glob wildcard: {path}");

        let matches = existing_matches(&path);
        assert_eq!(matches, vec![profile_dir], "glob must find the real profile directory through the Proton prefix");

        std::fs::remove_dir_all(&library).ok();
    }

    #[test]
    fn windows_only_rule_on_mac_never_attempts_proton() {
        let library = scratch_library("mac-oos");
        let game = fake_game(library.clone(), 999_003);

        let res = resolve_rule("<winAppData>/SomeGame", &windows_only_rule(), Os::Mac, &game)
            .expect("should still report something, just unresolvable");
        assert_eq!(res.via, Via::Native, "macOS has no Proton path — never claim Via::Proton");
        assert!(res.path.is_none());
        assert_eq!(res.note, Some("Windows-only; no Proton on macOS (out of scope)"));

        std::fs::remove_dir_all(&library).ok();
    }

    #[test]
    fn native_linux_rule_does_not_go_through_proton_mapping() {
        // A rule with no `when` constraints (or an explicit Linux/xdg
        // constraint) applies natively — it must never be treated as
        // "windows-only" and routed through the Proton prefix.
        let library = scratch_library("native-linux");
        let game = fake_game(library.clone(), 999_004);
        let rule = FileRule { tags: vec!["save".to_owned()], when: Vec::new() };

        let res = resolve_rule("<xdgData>/somegame/saves", &rule, Os::Linux, &game)
            .expect("unconstrained rule should resolve natively");
        assert_eq!(res.via, Via::Native);
        let path = res.path.expect("xdgData should always substitute");
        assert!(!path.contains("compatdata"), "must not be routed through the Proton prefix: {path}");

        std::fs::remove_dir_all(&library).ok();
    }
}
