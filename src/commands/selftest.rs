//! `selftest`: an end-to-end round trip against the real OneDrive.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};

use crate::resolve::Os;
use crate::store::Store;
use crate::{manifest, onedrive, snapshot, steam, sync};

/// End-to-end roundtrip against the real cloud using a synthetic game:
/// capture → upload → unchanged-detection → mutate → upload → delete local →
/// restore → verify. Cleans up after itself.
pub(crate) fn selftest() -> Result<()> {
    let os = Os::current();
    let tmp = snapshot::staging_dir()?.join("selftest");
    let library = tmp.join("library");
    let save_dir = library.join("steamapps/common/SelftestGame/SavesDir");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(save_dir.join("nested"))?;
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v1")?;
    std::fs::write(save_dir.join("nested/slot2.sav"), b"nested save")?;

    let game = steam::InstalledGame {
        app_id: 999_999_999,
        name: "YASGM Selftest".to_owned(),
        install_dir: "SelftestGame".to_owned(),
        library: library.clone(),
    };
    let mut files = HashMap::new();
    files.insert(
        "<base>/SavesDir".to_owned(),
        manifest::FileRule { tags: vec!["save".to_owned()], when: Vec::new() },
    );

    let store = Store::new(
        Box::new(onedrive::OneDriveProvider::new()),
        "selftest".to_owned(),
    )?;

    println!("1/7 capture + upload…");
    let snap = snapshot::capture(&files, &game, os)?.context("capture found nothing")?;
    let first = store.push(&game.name, game.app_id, &snap, os.name(), 3, false)?;
    println!("      uploaded {}", first.id);

    println!("2/7 unchanged detection…");
    let snap2 = snapshot::capture(&files, &game, os)?.context("recapture found nothing")?;
    ensure!(snap2.content_hash == first.content_hash, "hash changed for identical state");
    let _ = std::fs::remove_file(&snap2.zip_path);

    println!("3/7 mutate + second version…");
    std::thread::sleep(std::time::Duration::from_secs(1)); // distinct version id
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v2 CHANGED")?;
    let snap3 = snapshot::capture(&files, &game, os)?.context("capture found nothing")?;
    ensure!(snap3.content_hash != first.content_hash, "hash identical after mutation");
    let second = store.push(&game.name, game.app_id, &snap3, os.name(), 3, false)?;
    println!("      uploaded {}", second.id);

    println!("4/7 delete local saves…");
    std::fs::remove_dir_all(&save_dir)?;

    println!("5/7 restore head from cloud…");
    let index = store.load_index(game.app_id)?.context("index missing")?;
    ensure!(index.versions.len() == 2, "expected 2 versions, found {}", index.versions.len());
    let head = Store::head(&index).context("no head")?.clone();
    ensure!(head.id == second.id, "head is not the second version");
    let bytes = store.download_version(game.app_id, &head)?;
    let written = snapshot::extract(&bytes, &head, &files, os, &game, false)?;
    ensure!(written == 2, "expected 2 restored files, wrote {written}");

    println!("6/7 verify contents…");
    let slot1 = std::fs::read_to_string(save_dir.join("slot1.sav"))?;
    ensure!(slot1 == "selftest save v2 CHANGED", "slot1 content mismatch: {slot1:?}");
    let slot2 = std::fs::read_to_string(save_dir.join("nested/slot2.sav"))?;
    ensure!(slot2 == "nested save", "slot2 content mismatch: {slot2:?}");

    println!("7/7 sync engine matrix…");
    let job = sync::Job { store: &store, files: &files, game: &game, os, backup_only: false, keep: 3, dry_run: false };
    fn last(hash: &str) -> Option<sync::LastSync<'_>> {
        Some(sync::LastSync { hash, local_hash: None })
    }
    // In sync: nothing changed since head.
    let (outcome, update) = sync::sync_game(&job, last(&head.content_hash))?;
    ensure!(matches!(outcome, sync::Outcome::InSync), "expected InSync, got {outcome:?}");
    let head_hash = update.context("InSync should update state")?.hash;
    // Local edit → upload.
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v3")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    let (outcome, update) = sync::sync_game(&job, last(&head_hash))?;
    ensure!(matches!(outcome, sync::Outcome::Uploaded(_)), "expected Uploaded, got {outcome:?}");
    let v3_hash = update.context("Uploaded should update state")?.hash;
    // Conflict: state says we last saw something neither side has; local
    // saves are the newest, so they win.
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(save_dir.join("slot1.sav"), b"selftest save v4 (divergent)")?;
    let (outcome, _) = sync::sync_game(&job, last("bogus-hash-neither-side"))?;
    let sync::Outcome::Conflict { pinned, local_won: true, .. } = &outcome else {
        anyhow::bail!("expected a conflict won by local, got {outcome:?}");
    };
    let index = store.load_index(game.app_id)?.context("index missing")?;
    ensure!(
        index.versions.iter().any(|v| &v.id == pinned && v.pinned),
        "conflict loser is not pinned in index"
    );
    // Cloud moved (as if from another machine): local unchanged → download.
    let (outcome, _) = sync::sync_game(&job, last(&v3_hash))?;
    ensure!(
        matches!(outcome, sync::Outcome::Downloaded { .. } | sync::Outcome::InSync),
        "expected Downloaded/InSync after cloud moved, got {outcome:?}"
    );

    println!("   conditional index write…");
    let session = onedrive::Session::new();
    let rel = format!("accounts/selftest/games/{}/index.json", game.app_id);
    let (bytes, tag) = session.download_tagged(&rel)?.context("index missing")?;
    ensure!(session.upload_if(&rel, &bytes, Some(&tag))?, "write with the current eTag was refused");
    ensure!(!session.upload_if(&rel, &bytes, Some(&tag))?, "write with a stale eTag was accepted");
    ensure!(!session.upload_if(&rel, &bytes, None)?, "create-only write over an existing file was accepted");

    session.delete("accounts/selftest")?;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("selftest passed ✔ (cloud + local artifacts cleaned up)");
    Ok(())
}
