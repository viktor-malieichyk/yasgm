//! Bidirectional sync engine (one game at a time).
//!
//! Three-state comparison: local capture vs last-synced state vs cloud head
//! (newest non-pinned version). Decision matrix:
//!
//!   local == head                     → in sync
//!   local changed, cloud didn't      → upload (new head)
//!   cloud changed, local didn't      → download head (preserving local
//!                                      first as a *pinned* version if the
//!                                      cloud no longer has its content)
//!   both changed / no state          → conflict (D5): pin the old head,
//!                                      upload local as the new head
//!
//! Backup-only games (Steam Cloud, D2) upload like everyone else but never
//! download automatically; a newer cloud head is only reported.

use std::collections::HashMap;

use anyhow::Result;

use crate::manifest::FileRule;
use crate::resolve::Os;
use crate::snapshot;
use crate::steam::InstalledGame;
use crate::store::{Store, Version};

#[derive(Debug)]
pub enum Outcome {
    NothingAnywhere,
    InSync,
    Uploaded(Version),
    Downloaded { version: Version, files_written: u64, local_preserved_as: Option<String> },
    CloudAhead(Version),
    Conflict { uploaded: Version, pinned: String },
    DryRun(&'static str),
}

/// New (version_id, content_hash) to record in state, if any.
pub type StateUpdate = Option<(String, String)>;

#[allow(clippy::too_many_arguments)]
pub fn sync_game(
    store: &Store,
    files: &HashMap<String, FileRule>,
    game: &InstalledGame,
    os: Os,
    backup_only: bool,
    last_hash: Option<&str>,
    keep: usize,
    dry_run: bool,
) -> Result<(Outcome, StateUpdate)> {
    // Best-effort: catch up any versions stranded locally from a previous
    // offline run before doing anything else, so a game whose save content
    // hasn't changed since (and so never reaches store.push below) still
    // gets its pending backup synced once connectivity returns.
    if !dry_run {
        let _ = store.flush_pending(game.app_id, keep);
    }

    let local = snapshot::capture(files, game, os)?;
    let index = store.load_index(game.app_id)?;
    let head = index.as_ref().and_then(Store::head).cloned();

    let discard = |snap: &snapshot::Snapshot| {
        let _ = std::fs::remove_file(&snap.zip_path);
    };

    match (local, head) {
        (None, None) => Ok((Outcome::NothingAnywhere, None)),

        // Only local data exists → first upload.
        (Some(snap), None) => {
            if dry_run {
                discard(&snap);
                return Ok((Outcome::DryRun("would upload first version"), None));
            }
            let version = store.push(&game.name, game.app_id, &snap, os.name(), keep, false)?;
            let update = Some((version.id.clone(), version.content_hash.clone()));
            Ok((Outcome::Uploaded(version), update))
        }

        // Only cloud data exists → safe to download (nothing to overwrite).
        (None, Some(head)) => {
            if backup_only {
                return Ok((Outcome::CloudAhead(head), None));
            }
            if dry_run {
                return Ok((Outcome::DryRun("would download head (no local data)"), None));
            }
            let bytes = store.download_version(game.app_id, &head)?;
            let files_written = snapshot::extract(&bytes, &head.mounts, os, game, false)?;
            let update = Some((head.id.clone(), head.content_hash.clone()));
            Ok((
                Outcome::Downloaded { version: head, files_written, local_preserved_as: None },
                update,
            ))
        }

        (Some(snap), Some(head)) => {
            if snap.content_hash == head.content_hash {
                discard(&snap);
                let update = Some((head.id.clone(), head.content_hash.clone()));
                return Ok((Outcome::InSync, update));
            }

            let local_unchanged = last_hash == Some(snap.content_hash.as_str());
            let cloud_unchanged = last_hash == Some(head.content_hash.as_str());

            if local_unchanged {
                // Cloud moved. Download, but never lose local content (D14):
                // if no cloud version holds it (e.g. pruned), preserve it as
                // a pinned version so it can't shadow the head.
                if backup_only {
                    discard(&snap);
                    return Ok((Outcome::CloudAhead(head), None));
                }
                if dry_run {
                    discard(&snap);
                    return Ok((Outcome::DryRun("would download newer cloud head"), None));
                }
                let have_local_in_cloud = index
                    .as_ref()
                    .is_some_and(|i| i.versions.iter().any(|v| v.content_hash == snap.content_hash));
                let preserved = if have_local_in_cloud {
                    discard(&snap);
                    None
                } else {
                    let v = store.push(&game.name, game.app_id, &snap, os.name(), keep, true)?;
                    Some(v.id)
                };
                let bytes = store.download_version(game.app_id, &head)?;
                let files_written = snapshot::extract(&bytes, &head.mounts, os, game, false)?;
                let update = Some((head.id.clone(), head.content_hash.clone()));
                return Ok((
                    Outcome::Downloaded {
                        version: head,
                        files_written,
                        local_preserved_as: preserved,
                    },
                    update,
                ));
            }

            if cloud_unchanged {
                // Local moved → upload becomes the new head.
                if dry_run {
                    discard(&snap);
                    return Ok((Outcome::DryRun("would upload local changes"), None));
                }
                let version = store.push(&game.name, game.app_id, &snap, os.name(), keep, false)?;
                let update = Some((version.id.clone(), version.content_hash.clone()));
                return Ok((Outcome::Uploaded(version), update));
            }

            // Both sides changed since we last saw them (or this machine has
            // no state yet): conflict per D5 — keep both. Local becomes the
            // active head; the old head is pinned until manually deleted.
            if dry_run {
                discard(&snap);
                return Ok((Outcome::DryRun("conflict: would pin cloud head and upload local"), None));
            }
            store.set_pinned(game.app_id, &head.id, true)?;
            let version = store.push(&game.name, game.app_id, &snap, os.name(), keep, false)?;
            let update = Some((version.id.clone(), version.content_hash.clone()));
            Ok((Outcome::Conflict { uploaded: version, pinned: head.id }, update))
        }
    }
}
