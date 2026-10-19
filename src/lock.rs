//! Single-daemon guard: at most one `watch` process per user, whichever way
//! it was started (autostart `watch --tray`, a terminal, the Decky plugin's
//! backend). Two watchers over the same saves would race each other's
//! uploads and state.json writes.
//!
//! Uses an OS advisory lock (flock / LockFileEx) on a fixed file in the data
//! dir, held for the daemon's lifetime. The OS releases it when the process
//! dies, so a crash never leaves a stale lock behind — unlike a bare PID
//! file. The PID goes into a separate sidecar file purely for messages:
//! Windows file locks also block reads of the locked file itself.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Held for as long as the daemon runs; dropping it releases the lock.
pub struct DaemonLock {
    _file: File,
    pid_path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
    }
}

/// Another daemon holds the lock. `pid` is best-effort (from the sidecar).
#[derive(Debug)]
pub struct AlreadyRunning {
    pub pid: Option<u32>,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "another yasgm watch daemon is already running (pid {pid})"),
            None => write!(f, "another yasgm watch daemon is already running"),
        }
    }
}

impl std::error::Error for AlreadyRunning {}

fn lock_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data directory on this platform")?
        .join("yasgm");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn acquire_daemon() -> Result<DaemonLock> {
    acquire_in(&lock_dir()?)
}

/// Whether some process currently holds the daemon lock, and its PID if
/// known. Probing briefly takes the lock when free, then releases it.
pub fn daemon_status() -> Result<Option<Option<u32>>> {
    match acquire_in(&lock_dir()?) {
        Ok(_probe) => Ok(None),
        Err(err) => match err.downcast::<AlreadyRunning>() {
            Ok(running) => Ok(Some(running.pid)),
            Err(err) => Err(err),
        },
    }
}

fn acquire_in(dir: &Path) -> Result<DaemonLock> {
    let lock_path = dir.join("watch.lock");
    let pid_path = dir.join("watch.pid");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let pid = fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse().ok());
            return Err(AlreadyRunning { pid }.into());
        }
        Err(TryLockError::Error(err)) => {
            return Err(err).with_context(|| format!("locking {}", lock_path.display()));
        }
    }
    let _ = fs::write(&pid_path, std::process::id().to_string());
    Ok(DaemonLock { _file: file, pid_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yasgm-lock-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_fails_with_pid_until_first_is_dropped() {
        let dir = scratch_dir("exclusive");
        let first = acquire_in(&dir).unwrap();

        let err = acquire_in(&dir).err().expect("second acquire must fail");
        let running = err.downcast::<AlreadyRunning>().expect("AlreadyRunning error");
        assert_eq!(running.pid, Some(std::process::id()));

        drop(first);
        assert!(!dir.join("watch.pid").exists(), "pid sidecar removed on drop");
        // Other tests spawn processes in parallel; a child forked while our
        // fd was open shares the flock until it execs (O_CLOEXEC then drops
        // it), so the release can lag by a moment. Retry briefly.
        let mut again = acquire_in(&dir);
        for _ in 0..50 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            again = acquire_in(&dir);
        }
        again.expect("lock is free again after drop");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_pid_file_without_lock_holder_does_not_block() {
        // A crashed daemon leaves its pid file behind but the OS has already
        // released its lock; that must not count as "running".
        let dir = scratch_dir("stale");
        fs::write(dir.join("watch.pid"), "999999").unwrap();
        let lock = acquire_in(&dir).expect("stale pid file is ignored");
        assert_eq!(
            fs::read_to_string(dir.join("watch.pid")).unwrap(),
            std::process::id().to_string()
        );
        drop(lock);
        fs::remove_dir_all(&dir).unwrap();
    }
}
