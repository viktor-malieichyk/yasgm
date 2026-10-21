//! Bidirectional sync engine (one game at a time).
//!
//! Three-state comparison: local capture vs what this machine last synced
//! (state.json) vs the cloud. Decision matrix:
//!
//!   local == head (and nobody else moved) → in sync
//!   local changed, cloud didn't          → upload (new head)
//!   cloud changed, local didn't          → download head (preserving local
//!                                          first as a *pinned* version if
//!                                          the cloud no longer has it)
//!   both changed / no state              → conflict (D5): keep both; the
//!                                          side whose saves were written
//!                                          last becomes active, the other
//!                                          is pinned
//!
//! "The cloud changed" means *another* machine added a version: versions
//! this machine made offline and only just uploaded (`flush_pending`) don't
//! count, or a backup made on a plane would silently beat progress made
//! elsewhere in the meantime.
//!
//! Backup-only games (Steam Cloud, D2) upload like everyone else but never
//! download automatically; a newer cloud head is only reported, and in a
//! conflict the local saves always become active.

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
    /// Both sides changed. `active` is the version now in use (uploaded from
    /// here if `local_won`, else downloaded); `pinned` holds the other side.
    Conflict { active: Version, pinned: String, local_won: bool, files_written: u64 },
    DryRun(&'static str),
}

/// What this machine last synced for a game (from state.json).
#[derive(Debug, Clone, Copy)]
pub struct LastSync<'a> {
    /// Content hash of the cloud version we last matched.
    pub hash: &'a str,
    /// Hash of the local files right after that, when they differ from the
    /// version (a download that skipped a location, or local-only files
    /// that a download doesn't delete) — so that difference isn't mistaken
    /// for a local change and uploaded.
    pub local_hash: Option<&'a str>,
}

impl LastSync<'_> {
    fn matches_local(&self, hash: &str) -> bool {
        self.hash == hash || self.local_hash == Some(hash)
    }
}

/// New state to record for the game.
#[derive(Debug, Clone, PartialEq)]
pub struct StateUpdate {
    pub version: String,
    pub hash: String,
    pub local_hash: Option<String>,
}

impl StateUpdate {
    fn of(version: &Version, local_hash: Option<String>) -> Option<StateUpdate> {
        // A version still waiting in the pending store isn't synced: keep
        // the last cloud state, so the next online run can tell whether
        // another machine moved on meanwhile.
        (!version.pending).then(|| StateUpdate {
            version: version.id.clone(),
            hash: version.content_hash.clone(),
            local_hash: local_hash.filter(|h| *h != version.content_hash),
        })
    }
}

/// One game to sync, and how.
pub struct Job<'a> {
    pub store: &'a Store,
    pub files: &'a HashMap<String, FileRule>,
    pub game: &'a InstalledGame,
    pub os: Os,
    pub backup_only: bool,
    pub keep: usize,
    pub dry_run: bool,
}

impl Job<'_> {
    fn upload(&self, snap: &snapshot::Snapshot, pinned: bool) -> Result<Version> {
        crate::activity::set("uploading");
        self.store.push(&self.game.name, self.game.app_id, snap, self.os.name(), self.keep, pinned)
    }

    /// Downloads `version` over the local saves; returns the files written
    /// and the local hash afterwards (see `LastSync::local_hash`).
    fn download(&self, version: &Version) -> Result<(u64, Option<String>)> {
        crate::activity::set("downloading");
        let bytes = self.store.download_version(self.game.app_id, version)?;
        let written = snapshot::extract(&bytes, version, self.files, self.os, self.game, false)?;
        Ok((written, local_hash(self.files, self.game, self.os)?))
    }
}

/// Hash of the game's local saves right now, if it has any.
pub fn local_hash(files: &HashMap<String, FileRule>, game: &InstalledGame, os: Os) -> Result<Option<String>> {
    Ok(snapshot::capture(files, game, os)?.map(|snap| {
        let _ = std::fs::remove_file(&snap.zip_path);
        snap.content_hash
    }))
}

pub fn sync_game(job: &Job, last: Option<LastSync>) -> Result<(Outcome, Option<StateUpdate>)> {
    let Job { store, game, backup_only, dry_run, .. } = *job;
    // Upload versions stranded by an earlier offline run first, so this
    // run sees them in the cloud. Still offline is fine: they stay pending.
    let flushed = if dry_run {
        Vec::new()
    } else {
        store.flush_pending(game.app_id, job.keep).unwrap_or_else(|err| {
            eprintln!("{}: offline backups not uploaded yet ({err:#})", game.name);
            Vec::new()
        })
    };

    crate::activity::set("checking");
    let local = snapshot::capture(job.files, game, job.os)?;
    let index = store.load_index(game.app_id)?;
    let versions: &[Version] = index.as_ref().map_or(&[], |i| &i.versions);
    let head = index.as_ref().and_then(Store::head).cloned();
    // The newest version from the cloud's point of view: not one of ours
    // that's still pending or was pending until a moment ago.
    let cloud_head = versions
        .iter()
        .filter(|v| !v.pinned && !v.pending && !flushed.contains(&v.id))
        .max_by_key(|v| v.created)
        .cloned();

    let discard = |snap: &snapshot::Snapshot| {
        let _ = std::fs::remove_file(&snap.zip_path);
    };

    let (snap, head) = match (local, head) {
        (None, None) => return Ok((Outcome::NothingAnywhere, None)),

        // Only local data exists → first upload.
        (Some(snap), None) => {
            if dry_run {
                discard(&snap);
                return Ok((Outcome::DryRun("would upload first version"), None));
            }
            let version = job.upload(&snap, false)?;
            let update = StateUpdate::of(&version, None);
            return Ok((Outcome::Uploaded(version), update));
        }

        // Only cloud data exists → safe to download (nothing to overwrite).
        (None, Some(head)) => {
            if backup_only {
                return Ok((Outcome::CloudAhead(head), None));
            }
            if dry_run {
                return Ok((Outcome::DryRun("would download head (no local data)"), None));
            }
            let (files_written, local) = job.download(&head)?;
            let update = StateUpdate::of(&head, local);
            return Ok((Outcome::Downloaded { version: head, files_written, local_preserved_as: None }, update));
        }

        (Some(snap), Some(head)) => (snap, head),
    };

    let local_unchanged = last.is_some_and(|l| l.matches_local(&snap.content_hash));
    let cloud_unchanged = match &cloud_head {
        None => true, // only our own versions
        Some(c) => last.is_some_and(|l| l.hash == c.content_hash),
    };

    if snap.content_hash == head.content_hash
        && (cloud_unchanged || cloud_head.as_ref().is_some_and(|c| c.content_hash == snap.content_hash))
    {
        discard(&snap);
        return Ok((Outcome::InSync, StateUpdate::of(&head, None)));
    }
    if local_unchanged && last.is_some_and(|l| l.hash == head.content_hash) {
        // Local differs from the head only as it did right after we synced it.
        discard(&snap);
        return Ok((Outcome::InSync, None));
    }

    if local_unchanged {
        // Cloud moved. Download, but never lose local content (D14): if no
        // cloud version holds it (e.g. pruned), preserve it as a pinned
        // version so it can't shadow the head.
        if backup_only {
            discard(&snap);
            return Ok((Outcome::CloudAhead(head), None));
        }
        if dry_run {
            discard(&snap);
            return Ok((Outcome::DryRun("would download newer cloud head"), None));
        }
        let preserved = if versions.iter().any(|v| v.content_hash == snap.content_hash) {
            discard(&snap);
            None
        } else {
            Some(job.upload(&snap, true)?.id)
        };
        let (files_written, local) = job.download(&head)?;
        let update = StateUpdate::of(&head, local);
        return Ok((Outcome::Downloaded { version: head, files_written, local_preserved_as: preserved }, update));
    }

    if cloud_unchanged {
        // Local moved → upload becomes the new head.
        if dry_run {
            discard(&snap);
            return Ok((Outcome::DryRun("would upload local changes"), None));
        }
        let version = job.upload(&snap, false)?;
        let update = StateUpdate::of(&version, None);
        return Ok((Outcome::Uploaded(version), update));
    }

    // Both sides changed since we last saw them (or this machine has no
    // state yet): conflict per D5 — keep both. The side whose saves were
    // written last becomes active; the other is pinned until deleted.
    let other = cloud_head.expect("the cloud changed, so it has a head");
    let other_time = other.saved_at.unwrap_or(other.created);
    let local_wins = backup_only || snap.saved_at.is_none_or(|t| t >= other_time);
    if dry_run {
        discard(&snap);
        return Ok((
            Outcome::DryRun(if local_wins {
                "conflict: would pin the cloud's version and make local active"
            } else {
                "conflict: would pin local and download the cloud's newer version"
            }),
            None,
        ));
    }
    // A version that already holds the local saves (e.g. one of ours just
    // uploaded from the pending store) is reused rather than duplicated.
    let local_version = versions.iter().filter(|v| v.content_hash == snap.content_hash && v.id != other.id).max_by_key(|v| v.created);

    if local_wins {
        store.set_pinned(game.app_id, &other.id, true)?;
        let reusable = local_version.filter(|v| {
            !v.pinned && versions.iter().filter(|o| !o.pinned && o.id != other.id).all(|o| o.created <= v.created)
        });
        let active = match reusable {
            Some(v) => {
                discard(&snap);
                v.clone()
            }
            None => job.upload(&snap, false)?,
        };
        let update = StateUpdate::of(&active, None);
        return Ok((Outcome::Conflict { active, pinned: other.id, local_won: true, files_written: 0 }, update));
    }

    let pinned = match local_version {
        Some(v) => {
            discard(&snap);
            store.set_pinned(game.app_id, &v.id, true)?;
            v.id.clone()
        }
        None => job.upload(&snap, true)?.id,
    };
    let (files_written, local) = job.download(&other)?;
    let update = StateUpdate::of(&other, local);
    Ok((Outcome::Conflict { active: other, pinned, local_won: false, files_written }, update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalFolderProvider;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const APP: u64 = 4242;

    /// One machine: its own game folder and pending store, a shared cloud
    /// folder, and its own state.json entry.
    struct Machine {
        root: PathBuf,
        game: InstalledGame,
        files: HashMap<String, FileRule>,
        store: Store,
        last: Option<StateUpdate>,
    }

    struct Offline;
    impl crate::provider::Provider for Offline {
        fn exists(&self, _: &str) -> Result<bool> {
            anyhow::bail!("offline")
        }
        fn download(&self, _: &str) -> Result<Vec<u8>> {
            anyhow::bail!("offline")
        }
        fn upload(&self, _: &str, _: &[u8]) -> Result<()> {
            anyhow::bail!("offline")
        }
        fn delete(&self, _: &str) -> Result<()> {
            anyhow::bail!("offline")
        }
        fn list(&self, _: &str) -> Result<Vec<String>> {
            anyhow::bail!("offline")
        }
    }

    fn scratch(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "yasgm-sync-test-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    impl Machine {
        fn new(cloud: &Path) -> Machine {
            let root = scratch("machine");
            let library = root.join("library");
            let game = InstalledGame { app_id: APP, name: "Test Game".into(), install_dir: "Game".into(), library };
            let mut files = HashMap::new();
            files.insert("<base>/Saves".to_owned(), FileRule { tags: vec!["save".into()], when: Vec::new() });
            let store = Store::with_providers(
                Box::new(LocalFolderProvider::new(cloud.to_path_buf())),
                Box::new(LocalFolderProvider::new(root.join("pending"))),
                "acct".into(),
            );
            Machine { root, game, files, store, last: None }
        }

        fn go_offline(&mut self) {
            self.store = Store::with_providers(
                Box::new(Offline),
                Box::new(LocalFolderProvider::new(self.root.join("pending"))),
                "acct".into(),
            );
        }

        fn go_online(&mut self, cloud: &Path) {
            self.store = Store::with_providers(
                Box::new(LocalFolderProvider::new(cloud.to_path_buf())),
                Box::new(LocalFolderProvider::new(self.root.join("pending"))),
                "acct".into(),
            );
        }

        fn saves(&self) -> PathBuf {
            self.game.library.join("steamapps/common/Game/Saves")
        }

        /// Writes the save file with a given modification time.
        fn play(&self, content: &str, mtime: u64) {
            std::fs::create_dir_all(self.saves()).unwrap();
            let path = self.saves().join("slot.sav");
            std::fs::write(&path, content).unwrap();
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(time).unwrap();
        }

        fn save_content(&self) -> String {
            std::fs::read_to_string(self.saves().join("slot.sav")).unwrap_or_default()
        }

        fn sync_as(&mut self, backup_only: bool) -> Outcome {
            let job = Job {
                store: &self.store,
                files: &self.files,
                game: &self.game,
                os: Os::current(),
                backup_only,
                keep: 10,
                dry_run: false,
            };
            let last = self.last.as_ref().map(|l| LastSync { hash: &l.hash, local_hash: l.local_hash.as_deref() });
            let (outcome, update) = sync_game(&job, last).unwrap();
            if update.is_some() {
                self.last = update;
            }
            outcome
        }

        fn sync(&mut self) -> Outcome {
            self.sync_as(false)
        }

        fn versions(&self) -> Vec<Version> {
            self.store.load_index(APP).unwrap().map(|i| i.versions).unwrap_or_default()
        }

        fn head_content(&self) -> String {
            let index = self.store.load_index(APP).unwrap().unwrap();
            let head = Store::head(&index).unwrap();
            zip_content(&self.store.download_version(APP, head).unwrap())
        }
    }

    fn zip_content(bytes: &[u8]) -> String {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut entry = archive.by_index(0).unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(&mut entry, &mut out).unwrap();
        out
    }

    /// Version ids have one-second resolution.
    fn next_second() {
        std::thread::sleep(std::time::Duration::from_millis(1050));
    }

    fn setup() -> (PathBuf, Machine, Machine) {
        let cloud = scratch("cloud");
        let (pc, deck) = (Machine::new(&cloud), Machine::new(&cloud));
        (cloud, pc, deck)
    }

    #[test]
    fn nothing_anywhere_then_first_upload_then_in_sync() {
        let (_cloud, mut pc, _) = setup();
        assert!(matches!(pc.sync(), Outcome::NothingAnywhere));
        pc.play("v1", 1_760_000_000);
        assert!(matches!(pc.sync(), Outcome::Uploaded(_)));
        assert!(matches!(pc.sync(), Outcome::InSync));
        assert_eq!(pc.versions().len(), 1);
    }

    #[test]
    fn local_change_uploads_and_other_machine_downloads() {
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("v1", 1_760_000_000);
        pc.sync();
        assert!(matches!(deck.sync(), Outcome::Downloaded { local_preserved_as: None, .. }));
        assert_eq!(deck.save_content(), "v1");
        next_second();
        pc.play("v2", 1_760_000_100);
        assert!(matches!(pc.sync(), Outcome::Uploaded(_)));
        assert!(matches!(deck.sync(), Outcome::Downloaded { .. }));
        assert_eq!(deck.save_content(), "v2");
        assert!(matches!(deck.sync(), Outcome::InSync));
    }

    #[test]
    fn download_restores_the_file_time() {
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("v1", 1_760_000_000);
        pc.sync();
        deck.sync();
        let modified = std::fs::metadata(deck.saves().join("slot.sav")).unwrap().modified().unwrap();
        assert_eq!(modified, std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000));
    }

    #[test]
    fn backup_only_reports_a_newer_cloud_version_instead_of_downloading() {
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("v1", 1_760_000_000);
        pc.sync_as(true);
        assert!(matches!(deck.sync_as(true), Outcome::CloudAhead(_)));
        assert_eq!(deck.save_content(), "", "never downloaded");
        deck.play("v0", 1_759_900_000);
        deck.last = pc.last.clone(); // as if it had synced v1 before
        deck.last.as_mut().unwrap().local_hash = local_hash(&deck.files, &deck.game, Os::current()).unwrap();
        next_second();
        pc.play("v2", 1_760_000_100);
        pc.sync_as(true);
        assert!(matches!(deck.sync_as(true), Outcome::CloudAhead(_)));
        assert_eq!(deck.save_content(), "v0");
    }

    #[test]
    fn conflict_newer_local_saves_win_and_cloud_is_pinned() {
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("pc", 1_760_000_000);
        pc.sync();
        next_second();
        deck.play("deck newer", 1_760_000_500); // never synced: no state → conflict
        let Outcome::Conflict { local_won: true, pinned, .. } = deck.sync() else { panic!("expected local win") };
        assert_eq!(deck.head_content(), "deck newer");
        assert!(deck.versions().iter().any(|v| v.id == pinned && v.pinned));
        assert_eq!(deck.save_content(), "deck newer");
    }

    #[test]
    fn conflict_older_local_saves_lose_and_are_pinned() {
        // A new device with old saves lying around must not make them active.
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("pc newer", 1_760_000_500);
        pc.sync();
        deck.play("deck stale", 1_760_000_000);
        let Outcome::Conflict { local_won: false, pinned, .. } = deck.sync() else { panic!("expected cloud win") };
        assert_eq!(deck.save_content(), "pc newer", "the newer saves are now local");
        assert_eq!(deck.head_content(), "pc newer");
        let pinned_version = deck.versions().into_iter().find(|v| v.id == pinned).unwrap();
        assert!(pinned_version.pinned);
        assert_eq!(zip_content(&deck.store.download_version(APP, &pinned_version).unwrap()), "deck stale");
        // Settled: both machines agree afterwards.
        assert!(matches!(deck.sync(), Outcome::InSync));
        assert!(matches!(pc.sync(), Outcome::InSync));
    }

    #[test]
    fn backup_only_conflict_keeps_local_active() {
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("pc newer", 1_760_000_500);
        pc.sync_as(true);
        deck.play("deck older", 1_760_000_000);
        assert!(matches!(deck.sync_as(true), Outcome::Conflict { local_won: true, .. }));
        assert_eq!(deck.save_content(), "deck older", "backup-only never overwrites local saves");
    }

    #[test]
    fn offline_backup_is_uploaded_later_without_taking_over() {
        // The deck plays offline; meanwhile the PC uploads newer progress.
        let (cloud, mut pc, mut deck) = setup();
        pc.play("base", 1_760_000_000);
        pc.sync();
        deck.sync();
        deck.go_offline();
        deck.play("deck offline", 1_760_000_100);
        let Outcome::Uploaded(v) = deck.sync() else { panic!("expected a pending upload") };
        assert!(v.pending);
        next_second();
        pc.play("pc newer", 1_760_000_200);
        pc.sync();
        next_second();

        deck.go_online(&cloud);
        let outcome = deck.sync();
        let Outcome::Conflict { local_won: false, pinned, .. } = outcome else { panic!("expected a conflict, got {outcome:?}") };
        // The offline version itself is pinned, not a duplicate. (Both test
        // machines share a name, so its id may have gained a suffix.)
        assert!(pinned.starts_with(&v.id), "{pinned} vs {}", v.id);
        assert_eq!(deck.save_content(), "pc newer");
        let offline = deck.versions().into_iter().find(|x| x.id == v.id).unwrap();
        assert_eq!(offline.created, v.created, "keeps its original date");
        assert_eq!(deck.versions().len(), 3);
        assert!(matches!(pc.sync(), Outcome::InSync));
    }

    #[test]
    fn offline_backup_newer_than_other_progress_wins_but_pins_it() {
        let (cloud, mut pc, mut deck) = setup();
        pc.play("base", 1_760_000_000);
        pc.sync();
        deck.sync();
        pc.play("pc", 1_760_000_100);
        pc.sync();
        next_second();
        deck.go_offline();
        deck.play("deck offline later", 1_760_000_200);
        deck.sync();
        next_second();

        deck.go_online(&cloud);
        let outcome = deck.sync();
        assert!(matches!(outcome, Outcome::Conflict { local_won: true, .. }), "got {outcome:?}");
        assert_eq!(deck.head_content(), "deck offline later");
        assert!(deck.versions().iter().any(|v| v.pinned && zip_content(&deck.store.download_version(APP, v).unwrap()) == "pc"));
        assert_eq!(deck.versions().len(), 3, "no duplicate of the offline version");
        assert!(matches!(pc.sync(), Outcome::Downloaded { .. }));
        assert_eq!(pc.save_content(), "deck offline later");
    }

    #[test]
    fn offline_backup_with_nothing_new_elsewhere_just_syncs() {
        let (cloud, mut pc, mut deck) = setup();
        pc.play("base", 1_760_000_000);
        pc.sync();
        deck.sync();
        deck.go_offline();
        deck.play("deck offline", 1_760_000_100);
        deck.sync();
        next_second();
        deck.go_online(&cloud);
        let outcome = deck.sync();
        assert!(matches!(outcome, Outcome::InSync), "got {outcome:?}");
        assert_eq!(deck.versions().len(), 2);
        assert!(matches!(pc.sync(), Outcome::Downloaded { .. }));
        assert_eq!(pc.save_content(), "deck offline");
    }

    #[test]
    fn local_only_files_left_after_a_download_are_not_uploaded_again() {
        // A download doesn't delete local files, so local can differ from
        // the version it just downloaded; that isn't a local change.
        let (_cloud, mut pc, mut deck) = setup();
        pc.play("v1", 1_760_000_000);
        pc.sync();
        deck.play("old", 1_759_900_000);
        std::fs::write(deck.saves().join("extra.sav"), "deck only").unwrap();
        let f = std::fs::File::options().write(true).open(deck.saves().join("extra.sav")).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_759_900_000)).unwrap();
        assert!(matches!(deck.sync(), Outcome::Conflict { local_won: false, .. }));
        assert!(deck.last.as_ref().unwrap().local_hash.is_some());
        assert!(matches!(deck.sync(), Outcome::InSync), "extra local file isn't a change");
        next_second();
        pc.play("v2", 1_760_000_100);
        pc.sync();
        assert!(matches!(deck.sync(), Outcome::Downloaded { .. }));
        let before = deck.versions().len();
        assert!(matches!(deck.sync(), Outcome::InSync));
        assert_eq!(deck.versions().len(), before);
    }
}
