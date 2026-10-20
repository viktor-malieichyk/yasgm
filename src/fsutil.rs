//! Crash-safe file writes. The watch daemon can be terminated at any moment
//! (updater stopping it, logout, `kill`), and an in-place `fs::write` cut
//! short leaves a truncated file — for state.json/config.json that silently
//! loads as defaults, for tokens.json it forces a re-sign-in.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Writes to a sibling temp file, then renames over `path` (atomic on the
/// same filesystem), so readers only ever see the old or the new content.
/// `prepare` runs on the temp file before the rename (e.g. tightening
/// permissions so the final file is never briefly world-readable).
pub fn write_atomic_with(path: &Path, bytes: &[u8], prepare: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = prepare(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with(path, bytes, |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("yasgm-fsutil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no leftover temp file");
        fs::remove_dir_all(&dir).unwrap();
    }
}
