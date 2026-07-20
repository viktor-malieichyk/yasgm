//! Snapshots: one zip per capture. Entries are grouped under per-root
//! "mounts" (`p0/…`, `p1/…`); each mount records the manifest path template
//! (and the concrete value any `*` wildcard had), so the same zip restores on
//! any OS by re-resolving the template locally. A root that is a single file
//! is stored as `<mount>/@file`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use std::collections::{HashMap, HashSet};

use crate::manifest::FileRule;
use crate::resolve::{self, Os};
use crate::steam::InstalledGame;

const FILE_SENTINEL: &str = "@file";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub mount: String,
    pub template: String,
    /// Concrete value the `*` wildcard (e.g. <storeUserId>) had at capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wildcard: Option<String>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub zip_path: PathBuf,
    pub content_hash: String,
    pub file_count: u64,
    pub size: u64,
    pub mounts: Vec<Mount>,
}

pub fn staging_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("no cache directory")?
        .join("yasgm/staging");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        out.push(root.to_path_buf());
    } else if root.is_dir() {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                walk_files(&entry.path(), out);
            }
        }
    }
}

/// Where a pattern segment contains `*`, return just the substring the
/// wildcard matched — not the whole segment, since the wildcard (e.g.
/// `<storeUserId>`, substituted as a bare `*`) may not occupy the full
/// segment on its own (e.g. a template like `Slot<storeUserId>Data`).
/// Returning the whole segment there would make the later
/// `resolved.replace('*', value)` on restore double up the surrounding
/// literal text instead of reconstructing the original path.
fn wildcard_value(pattern: &str, matched: &Path) -> Option<String> {
    if !pattern.contains('*') {
        return None;
    }
    let pattern_segments: Vec<&str> = pattern
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    let matched_string = matched.to_string_lossy();
    let matched_segments: Vec<&str> = matched_string
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    if pattern_segments.len() != matched_segments.len() {
        return None;
    }
    for (p, m) in pattern_segments.iter().zip(&matched_segments) {
        let Some((prefix, suffix)) = p.split_once('*') else { continue };
        if let Some(value) = m.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) {
            return Some(value.to_owned());
        }
    }
    None
}

struct Entry {
    zip_name: String,
    source: PathBuf,
}

fn collect_entries(
    files: &HashMap<String, FileRule>,
    game: &InstalledGame,
    os: Os,
) -> (Vec<Entry>, Vec<Mount>) {
    let mut templates: Vec<(&String, &FileRule)> = files.iter().collect();
    templates.sort_by_key(|(template, _)| template.as_str());

    let mut entries = Vec::new();
    let mut mounts = Vec::new();
    for (template, rule) in templates {
        if !rule.is_save() {
            continue;
        }
        let Some(res) = resolve::resolve_rule(template, rule, os, game) else {
            continue;
        };
        let Some(pattern) = res.path else { continue };
        for root in resolve::existing_matches(&pattern) {
            let mount_id = format!("p{}", mounts.len());
            let mut files = Vec::new();
            walk_files(&root, &mut files);
            if files.is_empty() {
                continue;
            }
            for file in files {
                let zip_name = if root.is_file() {
                    format!("{mount_id}/{FILE_SENTINEL}")
                } else {
                    let rel = file.strip_prefix(&root).unwrap_or(&file);
                    format!("{mount_id}/{}", rel.to_string_lossy().replace('\\', "/"))
                };
                entries.push(Entry { zip_name, source: file });
            }
            mounts.push(Mount {
                mount: mount_id,
                template: template.clone(),
                wildcard: wildcard_value(&pattern, &root),
            });
        }
    }
    entries.sort_by(|a, b| a.zip_name.cmp(&b.zip_name));
    (entries, mounts)
}

/// Capture a game's current save state into a zip in the staging dir.
/// Returns None when no save files exist locally.
pub fn capture(
    files: &HashMap<String, FileRule>,
    game: &InstalledGame,
    os: Os,
) -> Result<Option<Snapshot>> {
    let (entries, mounts) = collect_entries(files, game, os);
    if entries.is_empty() {
        return Ok(None);
    }

    // Deterministic content hash over names + bytes, so identical states are
    // recognized regardless of file mtimes.
    let mut hasher = Sha256::new();
    let zip_path = staging_dir()?.join(format!("{}-capture.zip", game.app_id));
    let file = File::create(&zip_path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut file_count = 0u64;
    for entry in &entries {
        let mut content = Vec::new();
        File::open(&entry.source)
            .and_then(|mut f| f.read_to_end(&mut content))
            .with_context(|| format!("reading {}", entry.source.display()))?;
        hasher.update(entry.zip_name.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
        zip.start_file(entry.zip_name.clone(), options)?;
        zip.write_all(&content)?;
        file_count += 1;
    }
    zip.finish()?;

    let content_hash = format!("{:x}", hasher.finalize());
    let size = fs::metadata(&zip_path)?.len();
    Ok(Some(Snapshot { zip_path, content_hash, file_count, size, mounts }))
}

/// Extract a snapshot zip onto this machine by re-resolving each mount's
/// template. Never deletes local files; only writes/overwrites the files the
/// snapshot contains (callers snapshot current state first — D14).
///
/// A mount can fail to resolve on this OS (e.g. a Linux-native mount from a
/// snapshot captured on Linux, restored on Windows) — that's expected for
/// cross-OS restores, not a fatal error, so such mounts are skipped rather
/// than aborting the whole restore.
pub fn extract(
    zip_bytes: &[u8],
    mounts: &[Mount],
    os: Os,
    game: &InstalledGame,
    dry_run: bool,
) -> Result<u64> {
    let mut archive = ZipArchive::new(io::Cursor::new(zip_bytes))?;
    let mut written = 0u64;
    let mut skipped_mounts: HashSet<String> = HashSet::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let Some((mount_id, rel)) = name.split_once('/') else {
            bail!("malformed snapshot entry: {name}");
        };
        if rel.split('/').any(|seg| seg == "..") {
            bail!("snapshot entry escapes its mount: {name}");
        }
        let mount = mounts
            .iter()
            .find(|m| m.mount == mount_id)
            .with_context(|| format!("snapshot references unknown mount {mount_id}"))?;

        let Some(resolved) = resolve::resolve_template(&mount.template, os, game) else {
            if skipped_mounts.insert(mount_id.to_owned()) {
                eprintln!("  skipping {mount_id} — cannot resolve {} on this OS", mount.template);
            }
            continue;
        };
        let resolved = match (&mount.wildcard, resolved.contains('*')) {
            (Some(value), true) => resolved.replace('*', value),
            (None, true) => {
                if skipped_mounts.insert(mount_id.to_owned()) {
                    eprintln!(
                        "  skipping {mount_id} — unresolved wildcard in {} (no recorded value)",
                        mount.template
                    );
                }
                continue;
            }
            _ => resolved,
        };

        let target = if rel == FILE_SENTINEL {
            PathBuf::from(&resolved)
        } else {
            Path::new(&resolved).join(rel)
        };
        if dry_run {
            println!("  would write {}", target.display());
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&target)
                .with_context(|| format!("writing {}", target.display()))?;
            io::copy(&mut entry, &mut out)?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_value_full_segment() {
        let pattern = "C:/Users/X/Documents/MyGame/*/saves";
        let matched = Path::new("C:/Users/X/Documents/MyGame/76561198012345678/saves");
        assert_eq!(wildcard_value(pattern, matched), Some("76561198012345678".to_owned()));
    }

    #[test]
    fn wildcard_value_embedded_in_segment() {
        // The wildcard placeholder doesn't always occupy a whole path
        // segment on its own (e.g. `Slot<storeUserId>Data`) — only the
        // substituted portion should be captured, not the whole segment.
        let pattern = "C:/Users/X/Documents/MyGame/Slot*Data/saves";
        let matched = Path::new("C:/Users/X/Documents/MyGame/Slot76561198012345678Data/saves");
        assert_eq!(wildcard_value(pattern, matched), Some("76561198012345678".to_owned()));
    }

    #[test]
    fn wildcard_value_roundtrips_through_replace() {
        // This mirrors what actually happens: capture records the value via
        // wildcard_value, restore re-resolves the template (still containing
        // the literal `*`) and reconstructs the path via `.replace('*', ..)`.
        for (pattern, matched) in [
            ("MyGame/*/saves", "MyGame/76561198012345678/saves"),
            ("MyGame/Slot*Data/saves", "MyGame/Slot76561198012345678Data/saves"),
        ] {
            let value = wildcard_value(pattern, Path::new(matched)).expect("should capture a value");
            assert_eq!(pattern.replace('*', &value), matched);
        }
    }

    #[test]
    fn wildcard_value_none_without_wildcard() {
        let pattern = "C:/Users/X/Documents/MyGame/saves";
        let matched = Path::new("C:/Users/X/Documents/MyGame/saves");
        assert_eq!(wildcard_value(pattern, matched), None);
    }

    #[test]
    fn wildcard_value_none_on_segment_count_mismatch() {
        let pattern = "MyGame/*/saves";
        let matched = Path::new("MyGame/saves");
        assert_eq!(wildcard_value(pattern, matched), None);
    }
}
