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

    pub fn from_name(name: &str) -> Option<Os> {
        match name {
            "windows" => Some(Os::Windows),
            "linux" => Some(Os::Linux),
            "mac" => Some(Os::Mac),
            _ => None,
        }
    }

    /// For people: "Windows", "macOS", "Linux".
    pub fn label(self) -> &'static str {
        match self {
            Os::Windows => "Windows",
            Os::Linux => "Linux",
            Os::Mac => "macOS",
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

// ---- cross-OS location matching -------------------------------------------
//
// A snapshot records the manifest template each mount was captured from.
// Restoring it on another OS needs that OS's rule for the same saves. Rules
// are compared by their *location*: the directory path after the OS's base
// dir, with per-OS spellings normalized (see `location_parts`). Tiers, most
// to least certain:
//   1. same location
//   2. nested: one is a subfolder of the other (`Terraria` vs
//      `Terraria/Players`); restore adds or drops the extra folder level
//   3. same last two folder names, or the same distinctive last name
//      (`Supergiant Games/Hades` vs `Saved Games/Hades`)
//   4. the game has exactly one save location on each side
// Candidates are always the same game's rules, so a wrong guess picks the
// wrong folder of that game at worst, and restore snapshots first (D14).

/// Base directories that differ per OS. What follows the longest matching
/// one is the OS-independent part of a save location, e.g. `Baba_Is_You`
/// in both `<home>/Library/Application Support/Baba_Is_You` and
/// `<xdgData>/Baba_Is_You`.
const OS_BASE_DIRS: &[&str] = &[
    "<home>/Library/Application Support/",
    "<home>/Library/Preferences/",
    "<home>/Library/Caches/",
    "<home>/Library/Containers/",
    "<home>/Library/",
    "<home>/.local/share/",
    "<home>/.config/unity3d/",
    "<home>/.config/",
    "<home>/Documents/My Games/",
    "<home>/Documents/",
    "<home>/Saved Games/",
    "<home>/AppData/LocalLow/",
    "<home>/AppData/Roaming/",
    "<home>/AppData/Local/",
    "<home>/",
    "<xdgData>/",
    "<xdgConfig>/unity3d/",
    "<xdgConfig>/",
    "$XDG_DATA_HOME/",
    "$XDG_CONFIG_HOME/",
    "<winLocalAppDataLow>/",
    "<winLocalAppData>/",
    "<winAppData>/",
    "<winDocuments>/My Games/",
    "<winDocuments>/Saved Games/",
    "<winDocuments>/",
    "<winPublic>/",
    "<winProgramData>/",
    "<winDir>/",
];

/// Folder names too common to identify a game's saves on their own
/// (compared after `norm`).
const GENERIC_DIRS: &[&str] = &[
    "save", "saves", "savegame", "savegames", "savedata", "saved", "data", "userdata", "profiles", "profile", "config",
    "settings", "local", "remote", "game", "games", "user", "users", "default",
];

/// Is the last path segment a file (pattern) rather than a directory?
/// `*.sav`, `debug.celeste`, `settings.ini` are; `Baba_Is_You` isn't, nor
/// macOS's dotted folder names like `com.RedCandleGames.NineSols` (only
/// their `.plist` files are files).
fn looks_like_file(segment: &str) -> bool {
    let Some((stem, ext)) = segment.rsplit_once('.') else { return false };
    if stem.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let lower = segment.to_lowercase();
    if lower.starts_with("unity.") || lower.starts_with("com.") {
        return ext.eq_ignore_ascii_case("plist");
    }
    (1..=8).contains(&ext.len())
}

pub fn template_is_file(template: &str) -> bool {
    template.replace('\\', "/").rsplit('/').next().is_some_and(looks_like_file)
}

fn is_literal(segment: &str) -> bool {
    !segment.contains(['<', '*', '?', '{'])
}

fn norm(segment: &str) -> String {
    segment.chars().filter(|c| !matches!(c, ' ' | '_' | '-' | '.')).flat_map(char::to_lowercase).collect()
}

fn distinctive(segment: &str) -> bool {
    let n = norm(segment);
    is_literal(segment) && n.chars().count() >= 4 && !GENERIC_DIRS.contains(&n.as_str())
}

/// The OS-independent directory path of a save location, original case:
/// base dir stripped, a trailing file pattern dropped (`Game/*.sav` is the
/// `Game` location), a macOS app bundle's `X.app/Contents/Resources`
/// dropped (≙ the install dir elsewhere), a leading dot dropped, and macOS's
/// `unity.Company.Product` / `com.Company.Product` split into
/// `Company/Product`.
fn location_parts(template: &str) -> Vec<String> {
    let t = template.replace('\\', "/");
    let lower = t.to_lowercase();
    let rest = OS_BASE_DIRS
        .iter()
        .filter(|base| lower.starts_with(&base.to_lowercase()))
        .max_by_key(|base| base.len())
        .map_or(t.as_str(), |base| &t[base.len()..]);
    let mut parts: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
    if parts.last().is_some_and(|s| looks_like_file(s)) {
        parts.pop();
    }
    if let Some(i) = parts.windows(3).position(|w| {
        w[0].to_lowercase().ends_with(".app") && w[1].eq_ignore_ascii_case("contents") && w[2].eq_ignore_ascii_case("resources")
    }) {
        parts.drain(i..i + 3);
    }
    if let Some(first) = parts.first_mut() {
        *first = first.trim_start_matches('.').to_owned();
    }
    if let Some(first) = parts.first().cloned() {
        let lower = first.to_lowercase();
        if let Some(prefix) = ["unity.", "com."].iter().find(|p| lower.starts_with(**p)) {
            let split: Vec<String> = first[prefix.len()..].splitn(2, '.').map(str::to_owned).collect();
            parts.splice(0..1, split);
        }
    }
    parts.retain(|s| !s.is_empty());
    parts
}

fn location_key(template: &str) -> String {
    location_parts(template).join("/").to_lowercase()
}

fn same_parts(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_lowercase() == y.to_lowercase())
}

/// How a matched location's path changes for the mount being restored.
#[derive(Debug, Clone, PartialEq)]
enum Adjust {
    Same,
    /// The counterpart is a parent folder: go down into these subfolders.
    Into(Vec<String>),
    /// The counterpart is a subfolder: go up this many levels.
    Up(usize),
}

/// Match tier (1 most certain) of a source location against a candidate,
/// given how many distinct save locations each side's OS has (tier 4
/// needs exactly one on both).
fn match_location(src: &[String], dst: &[String], src_count: usize, dst_count: usize) -> Option<(u8, Adjust)> {
    if same_parts(src, dst) {
        return Some((1, Adjust::Same));
    }
    if !dst.is_empty() && dst.len() < src.len() && same_parts(&src[..dst.len()], dst) {
        let extra = &src[dst.len()..];
        if distinctive(&dst[dst.len() - 1]) && extra.iter().all(|s| is_literal(s)) {
            return Some((2, Adjust::Into(extra.to_vec())));
        }
    }
    if !src.is_empty() && src.len() < dst.len() && same_parts(&dst[..src.len()], src) && distinctive(&src[src.len() - 1]) {
        return Some((2, Adjust::Up(dst.len() - src.len())));
    }
    let literal = |parts: &[String]| -> Vec<String> { parts.iter().filter(|s| is_literal(s)).map(|s| norm(s)).collect() };
    match (literal(src).as_slice(), literal(dst).as_slice()) {
        ([.., a1, a2], [.., b1, b2]) if a1 == b1 && a2 == b2 => return Some((3, Adjust::Same)),
        ([.., a], [.., b]) if a == b && distinctive(a) => return Some((3, Adjust::Same)),
        _ => {}
    }
    (src_count == 1 && dst_count == 1).then_some((4, Adjust::Same))
}

/// Is this rule tied to particular OSes (as opposed to OS-neutral)?
fn os_specific(rule: &FileRule) -> bool {
    rule.when.iter().any(|c| c.os.is_some())
}

/// The OS a rule is constrained to, if exactly one.
fn rule_os(rule: &FileRule) -> Option<Os> {
    let mut oss = rule.when.iter().filter_map(|c| Os::from_name(c.os.as_deref()?));
    let first = oss.next()?;
    oss.all(|o| o == first).then_some(first)
}

/// OS-specific Steam save rules that apply natively on `os`.
fn save_rules_for(files: &HashMap<String, FileRule>, os: Os) -> Vec<&String> {
    files
        .iter()
        .filter(|(_, r)| r.is_save() && os_specific(r) && applies_on(r, os))
        .map(|(t, _)| t)
        .collect()
}

fn distinct_locations<'a>(templates: impl IntoIterator<Item = &'a String>) -> usize {
    templates.into_iter().map(|t| location_key(t)).collect::<std::collections::HashSet<_>>().len()
}

fn proton_prefix(game: &InstalledGame) -> PathBuf {
    game.library.join("steamapps/compatdata").join(game.app_id.to_string()).join("pfx")
}

/// Where a snapshot mount goes on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreTarget {
    /// Manifest template of the chosen location.
    pub template: String,
    /// Concrete path, possibly with `*` (a recorded wildcard fills it).
    pub path: String,
    /// `path` is a file pattern (files land in its directory when remapped).
    pub file_pattern: bool,
    /// Location differs from the one the mount was captured from.
    pub remapped: bool,
}

/// Where to restore a snapshot mount captured from `template` (on
/// `from_os`, if known) on this machine.
///
/// The mount's own rule is used as-is when it applies natively here (same
/// OS, or OS-neutral like `<base>/save`). Otherwise this OS's OS-specific
/// rules are tried by tier (see above); Proton rules only count once the
/// game has a prefix (it has been run under Proton). Within a tier, a
/// candidate whose data already exists wins, then native over Proton.
/// `None` when nothing matches: skip, never guess.
pub fn restore_target(
    template: &str,
    files: &HashMap<String, FileRule>,
    os: Os,
    from_os: Option<Os>,
    game: &InstalledGame,
) -> Option<RestoreTarget> {
    let own = files.get(template);
    let as_is = |path: String| RestoreTarget {
        template: template.to_owned(),
        path,
        file_pattern: template_is_file(template),
        remapped: false,
    };
    match own {
        Some(rule) if applies_on(rule, os) => return substitute(template, &native_vars(os, game)).map(as_is),
        // Rule no longer in the manifest: re-resolve as-is, but only when
        // nothing OS-specific is involved.
        None if location_key(template) == template.trim_matches('/').to_lowercase() => {
            return resolve_template(template, os, game).map(as_is);
        }
        _ => {}
    }
    let src = location_parts(template);
    let src_count = from_os
        .or_else(|| own.and_then(rule_os))
        .map_or(0, |from| distinct_locations(save_rules_for(files, from)));
    let has_prefix = proton_prefix(game).is_dir();
    let candidates: Vec<(&String, Via, String)> = files
        .iter()
        .filter(|(_, rule)| rule.is_save() && os_specific(rule))
        .filter_map(|(t, rule)| {
            let res = resolve_rule(t, rule, os, game)?;
            (res.via == Via::Native || has_prefix).then_some((t, res.via, res.path?))
        })
        .collect();
    let group_size = |via: Via| distinct_locations(candidates.iter().filter(|c| c.1 == via).map(|c| c.0));
    let mut ranked: Vec<(u8, bool, bool, &String, String, Adjust)> = candidates
        .iter()
        .filter_map(|(t, via, path)| {
            let (tier, adjust) = match_location(&src, &location_parts(t), src_count, group_size(*via))?;
            // A prefix means the game runs under Proton: its Windows
            // locations count as live even before any save exists.
            let on_disk = match via {
                Via::Native => !existing_matches(path).is_empty(),
                Via::Proton => true,
            };
            Some((tier, !on_disk, *via != Via::Native, *t, path.clone(), adjust))
        })
        .collect();
    ranked.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
    let (_, _, _, chosen, path, adjust) = ranked.into_iter().next()?;
    let file_pattern = template_is_file(chosen);
    Some(match adjust {
        Adjust::Same => RestoreTarget { template: chosen.clone(), path, file_pattern, remapped: true },
        Adjust::Into(_) | Adjust::Up(_) => {
            let mut dir = PathBuf::from(&path);
            if file_pattern {
                dir.pop();
            }
            match &adjust {
                Adjust::Into(extra) => extra.iter().for_each(|s| dir.push(s)),
                Adjust::Up(levels) => (0..*levels).for_each(|_| {
                    dir.pop();
                }),
                Adjust::Same => {}
            }
            RestoreTarget {
                template: chosen.clone(),
                path: dir.to_string_lossy().into_owned(),
                file_pattern: false,
                remapped: true,
            }
        }
    })
}

/// Every `(from, to, source template, matched template, tier)` mapping
/// between this game's OS-specific save rules (dev: `manifest-gaps
/// --explain`, to review what the tiers pair up).
pub fn explain_matches(files: &HashMap<String, FileRule>) -> Vec<(Os, Os, String, String, u8)> {
    let all = [Os::Windows, Os::Mac, Os::Linux];
    let mut out = Vec::new();
    for from in all {
        let src = save_rules_for(files, from);
        for to in all {
            let dst = save_rules_for(files, to);
            if from == to || src.is_empty() || dst.is_empty() {
                continue;
            }
            let (src_count, dst_count) = (distinct_locations(src.iter().copied()), distinct_locations(dst.iter().copied()));
            for s in &src {
                if dst.contains(s) {
                    continue; // same rule on both OSes
                }
                let sp = location_parts(s);
                let best = dst
                    .iter()
                    .filter_map(|d| Some((match_location(&sp, &location_parts(d), src_count, dst_count)?.0, *d)))
                    .min();
                if let Some((tier, d)) = best {
                    out.push((from, to, (*s).clone(), d.clone(), tier));
                }
            }
        }
    }
    out
}

/// OS pairs `(from, to)` for which none of this game's save locations on
/// `from` has a counterpart on `to`, i.e. a backup made on `from` can't be
/// restored on `to`. Only OSes with their own (OS-specific) save rules are
/// compared; Proton isn't considered (a Windows backup restores into a
/// Proton prefix by its own Windows rule).
pub fn cross_os_gaps(files: &HashMap<String, FileRule>) -> Vec<(Os, Os)> {
    let all = [Os::Windows, Os::Mac, Os::Linux];
    let mut gaps = Vec::new();
    for from in all {
        let src = save_rules_for(files, from);
        for to in all {
            let dst = save_rules_for(files, to);
            if from == to || src.is_empty() || dst.is_empty() {
                continue;
            }
            let (src_count, dst_count) = (distinct_locations(src.iter().copied()), distinct_locations(dst.iter().copied()));
            let bridged = src.iter().any(|s| {
                let sp = location_parts(s);
                dst.iter().any(|d| match_location(&sp, &location_parts(d), src_count, dst_count).is_some())
            });
            if !bridged {
                gaps.push((from, to));
            }
        }
    }
    gaps
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
        } else if path.is_dir()
            && let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), files, bytes);
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

    fn restore_pair(template: &str, files: &HashMap<String, FileRule>, os: Os, game: &InstalledGame) -> Option<(String, String)> {
        restore_target(template, files, os, None, game).map(|t| (t.template, t.path))
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

    fn rules(entries: &[(&str, &str)]) -> HashMap<String, FileRule> {
        entries.iter().map(|(t, os)| (t.to_string(), os_rule(os))).collect()
    }

    fn target_on(template: &str, files: &HashMap<String, FileRule>, os: Os) -> Option<String> {
        let game = fake_game(scratch_library("tiers"), 1);
        restore_pair(template, files, os, &game).map(|(t, _)| t)
    }

    #[test]
    fn tier1_matches_unity_locations_across_all_three_oses() {
        let files = rules(&[
            ("<home>/AppData/LocalLow/Jutsu Games/911 Operator", "windows"),
            ("<xdgConfig>/unity3d/Jutsu Games/911 Operator", "linux"),
            ("<home>/Library/Application Support/unity.Jutsu Games.911 Operator", "mac"),
            // Distractors so tier 3 (one rule per side) can't be what matched.
            ("<home>/AppData/LocalLow/Jutsu Games/Other", "windows"),
            ("<xdgConfig>/unity3d/Jutsu Games/Other", "linux"),
            ("<home>/Library/Application Support/Other", "mac"),
        ]);
        assert_eq!(
            target_on("<home>/AppData/LocalLow/Jutsu Games/911 Operator", &files, Os::Linux).as_deref(),
            Some("<xdgConfig>/unity3d/Jutsu Games/911 Operator")
        );
        assert_eq!(
            target_on("<xdgConfig>/unity3d/Jutsu Games/911 Operator", &files, Os::Mac).as_deref(),
            Some("<home>/Library/Application Support/unity.Jutsu Games.911 Operator")
        );
    }

    #[test]
    fn tier2_matches_when_one_os_adds_leading_folders() {
        let files = rules(&[
            ("<winAppData>/zog/hercules", "windows"),
            ("<winAppData>/zog/other", "windows"),
            ("<home>/.saves/zog/hercules", "linux"),
            ("<home>/.saves/zog/other", "linux"),
        ]);
        assert_eq!(target_on("<winAppData>/zog/hercules", &files, Os::Linux).as_deref(), Some("<home>/.saves/zog/hercules"));
    }

    #[test]
    fn tier3_pairs_the_only_rule_on_each_side_but_not_with_choices() {
        let one_each = rules(&[
            ("<winLocalAppData>/10_Second_Ninja", "windows"),
            ("<home>/Library/Application Support/com.yoyogames.macyoyorunner", "mac"),
        ]);
        assert_eq!(
            target_on("<winLocalAppData>/10_Second_Ninja", &one_each, Os::Mac).as_deref(),
            Some("<home>/Library/Application Support/com.yoyogames.macyoyorunner")
        );
        let two_on_mac = rules(&[
            ("<winLocalAppData>/10_Second_Ninja", "windows"),
            ("<home>/Library/Application Support/com.yoyogames.macyoyorunner", "mac"),
            ("<home>/Library/Application Support/Unrelated", "mac"),
        ]);
        assert!(target_on("<winLocalAppData>/10_Second_Ninja", &two_on_mac, Os::Mac).is_none());
    }

    #[test]
    fn file_patterns_match_their_directory_both_ways() {
        let files = rules(&[
            ("<home>/AppData/LocalLow/Monomi Park/Slime Rancher/*.sav", "windows"),
            ("<home>/AppData/LocalLow/Monomi Park/Slime Rancher/slimerancher.prf", "windows"),
            ("<home>/Library/Application Support/Monomi Park/Slime Rancher", "mac"),
            ("<xdgConfig>/unity3d/Fullbright/Tacoma/TacomaSave*.json", "linux"),
            ("<home>/AppData/LocalLow/Fullbright/Tacoma", "windows"),
        ]);
        assert_eq!(
            target_on("<home>/AppData/LocalLow/Monomi Park/Slime Rancher/*.sav", &files, Os::Mac).as_deref(),
            Some("<home>/Library/Application Support/Monomi Park/Slime Rancher")
        );
        assert_eq!(
            target_on("<home>/AppData/LocalLow/Fullbright/Tacoma", &files, Os::Linux).as_deref(),
            Some("<xdgConfig>/unity3d/Fullbright/Tacoma/TacomaSave*.json")
        );
    }

    #[test]
    fn bundle_ids_and_feral_user_folders_match() {
        let files = rules(&[
            ("<home>/AppData/LocalLow/RedCandleGames/NineSols", "windows"),
            ("<winAppData>/Other/Thing", "windows"),
            ("<home>/Library/Application Support/com.RedCandleGames.NineSols", "mac"),
            ("<home>/Library/Application Support/Unrelated", "mac"),
            ("<winAppData>/The Creative Assembly/Warhammer2/save_games", "windows"),
            (
                "<xdgData>/feral-interactive/Total War WARHAMMER II/SaveData/Steam Saves (<storeUserId>)/local/Warhammer2/save_games",
                "linux",
            ),
            ("<xdgData>/feral-interactive/Unrelated", "linux"),
        ]);
        assert_eq!(
            target_on("<home>/AppData/LocalLow/RedCandleGames/NineSols", &files, Os::Mac).as_deref(),
            Some("<home>/Library/Application Support/com.RedCandleGames.NineSols")
        );
        assert!(target_on("<winAppData>/The Creative Assembly/Warhammer2/save_games", &files, Os::Linux)
            .is_some_and(|t| t.ends_with("Warhammer2/save_games")));
    }

    #[test]
    fn gaps_report_os_pairs_with_no_counterpart() {
        let files = rules(&[
            ("<winAppData>/A", "windows"),
            ("<winAppData>/B", "windows"),
            ("<home>/Library/Application Support/C", "mac"),
            ("<home>/Library/Application Support/D", "mac"),
            ("<xdgData>/A", "linux"),
        ]);
        let gaps = cross_os_gaps(&files);
        assert!(gaps.contains(&(Os::Windows, Os::Mac)) && gaps.contains(&(Os::Mac, Os::Windows)));
        assert!(gaps.contains(&(Os::Linux, Os::Mac)));
        assert!(!gaps.contains(&(Os::Windows, Os::Linux)) && !gaps.contains(&(Os::Linux, Os::Windows)));
    }

    fn rule_with(oss: &[&str], store: Option<&str>) -> FileRule {
        FileRule {
            tags: vec!["save".to_owned()],
            when: oss.iter().map(|os| Constraint { os: Some(os.to_string()), store: store.map(str::to_owned) }).collect(),
        }
    }

    fn full_target(template: &str, files: &HashMap<String, FileRule>, os: Os, from: Os) -> Option<RestoreTarget> {
        let game = fake_game(scratch_library("real-games"), 1);
        restore_target(template, files, os, Some(from), &game)
    }

    #[test]
    fn hades_ignores_the_microsoft_store_rule_and_matches_by_name() {
        let mut files = rules(&[
            ("<home>/Library/Application Support/Supergiant Games/Hades", "mac"),
            ("<winDocuments>/Saved Games/Hades", "windows"),
        ]);
        files.insert(
            "<winLocalAppData>/Packages/SupergiantGamesLLC.Hades_q53c1yqmx7pha/SystemAppData/wgs".into(),
            rule_with(&["windows"], Some("microsoft")),
        );
        assert!(cross_os_gaps(&files).is_empty());
        let t = full_target("<winDocuments>/Saved Games/Hades", &files, Os::Mac, Os::Windows);
        assert_eq!(t.map(|t| t.template).as_deref(), Some("<home>/Library/Application Support/Supergiant Games/Hades"));
    }

    #[test]
    fn celeste_counts_locations_not_rules() {
        // Two Windows patterns in one folder are one location, so the only
        // location on each side pairs up.
        let files = rules(&[
            ("<base>/Saves/*.celeste", "windows"),
            ("<base>/Saves/debug.celeste", "windows"),
            ("<home>/Library/Application Support/Celeste/Saves", "mac"),
        ]);
        assert!(cross_os_gaps(&files).is_empty());
    }

    #[test]
    fn slay_the_spire_app_bundle_and_multi_os_rules() {
        let mut files = rules(&[("<base>/SlayTheSpire.app/Contents/Resources/saves", "mac")]);
        files.insert("<base>/saves".into(), rule_with(&["linux", "windows"], None));
        assert!(cross_os_gaps(&files).is_empty());
        let t = full_target("<base>/saves", &files, Os::Mac, Os::Windows).expect("mac counterpart");
        assert_eq!(t.template, "<base>/SlayTheSpire.app/Contents/Resources/saves");
        assert!(!t.path.contains('<'), "{}", t.path);
    }

    #[test]
    fn nested_locations_add_or_drop_the_extra_folder() {
        // Terraria: Windows/Mac keep the whole folder, Linux lists subfolders.
        let files = rules(&[
            ("<winDocuments>/My Games/Terraria", "windows"),
            ("<home>/Library/Application Support/Terraria", "mac"),
            ("<xdgData>/Terraria/Players/*.plr", "linux"),
            ("<xdgData>/Terraria/Worlds/*.wld", "linux"),
        ]);
        assert!(cross_os_gaps(&files).is_empty());
        // Linux Players/ → into Terraria/Players on the Mac.
        let t = full_target("<xdgData>/Terraria/Players/*.plr", &files, Os::Mac, Os::Linux).expect("mac parent");
        assert_eq!(t.template, "<home>/Library/Application Support/Terraria");
        assert!(t.path.ends_with("Terraria/Players") && !t.file_pattern, "{}", t.path);
        // Windows Terraria/ → up from Players/*.plr to Terraria on Linux.
        let t = full_target("<winDocuments>/My Games/Terraria", &files, Os::Linux, Os::Windows).expect("linux parent");
        assert!(t.path.ends_with("/Terraria") && !t.file_pattern, "{}", t.path);
    }

    #[test]
    fn rimworld_saves_subfolder_maps_into_the_windows_game_folder() {
        let files = rules(&[
            ("<home>/.config/unity3d/Ludeon Studios/RimWorld/Saves", "linux"),
            ("<home>/AppData/LocalLow/Ludeon Studios/RimWorld", "windows"),
            ("<home>/AppData/LocalLow/Ludeon Studios/RimWorld by Ludeon Studios/Saves", "windows"),
        ]);
        assert!(cross_os_gaps(&files).is_empty());
    }

    #[test]
    fn restore_maps_mac_location_to_linux_counterpart() {
        let game = fake_game(scratch_library("restore-mac-linux"), 736260);
        let (template, path) =
            restore_pair("<home>/Library/Application Support/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
                .expect("linux has a counterpart");
        assert_eq!(template, "<xdgData>/Baba_Is_You");
        assert!(!path.contains("Library"), "mac path leaked onto linux: {path}");
        assert!(path.ends_with("/Baba_Is_You"), "{path}");
    }

    #[test]
    fn restore_maps_windows_location_to_mac_counterpart() {
        let game = fake_game(scratch_library("restore-win-mac"), 736260);
        let (template, _) = restore_pair("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Mac, &game)
            .expect("mac has a counterpart");
        assert_eq!(template, "<home>/Library/Application Support/Baba_Is_You");
    }

    #[test]
    fn restore_prefers_native_linux_over_proton_without_a_prefix() {
        // A Windows-captured version restored on Linux where the game runs
        // natively (no compatdata prefix): native location, not the prefix.
        let game = fake_game(scratch_library("restore-win-linux"), 736260);
        let (template, path) = restore_pair("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
            .expect("linux has a counterpart");
        assert_eq!(template, "<xdgData>/Baba_Is_You");
        assert!(!path.contains("compatdata"), "{path}");
    }

    #[test]
    fn restore_uses_proton_prefix_when_game_runs_under_proton() {
        let library = scratch_library("restore-proton");
        let game = fake_game(library.clone(), 736260);
        std::fs::create_dir_all(proton_prefix(&game)).unwrap();
        let (template, path) = restore_pair("<winAppData>/Baba_Is_You", &per_os_rules(), Os::Linux, &game)
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
        assert!(restore_pair("<home>/Library/Application Support/MacOnly", &files, Os::Linux, &game).is_none());
        let (template, _) = restore_pair("<base>/save", &files, Os::Linux, &game).expect("neutral rule applies");
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
