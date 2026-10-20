//! Showing the YASGM window from the CLI side (the daemon's tray menu and
//! `yasgm gui`). The desktop app ships next to this binary — `yasgm` is its
//! sidecar — and is single-instance: launching it again just brings the
//! running window forward (on macOS, `open` on the bundle does that via the
//! app's Reopen handler).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// The desktop app's executable, if this binary is installed alongside one.
fn gui_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let names: &[&str] = if cfg!(windows) { &["ui.exe", "YASGM.exe"] } else { &["ui", "YASGM"] };
    names.iter().map(|name| dir.join(name)).find(|path| path.is_file())
}

pub fn available() -> bool {
    gui_binary().is_some()
}

/// Opens the YASGM window, starting the app if it isn't running.
pub fn open() -> Result<()> {
    let gui = gui_binary().context("the YASGM app isn't installed next to this program")?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = gui.ancestors().nth(3).filter(|p| p.extension().is_some_and(|e| e == "app")) {
        // …/YASGM.app/Contents/MacOS/ui
        Command::new("open").arg(bundle).spawn().context("running `open`")?;
        return Ok(());
    }
    Command::new(&gui)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", gui.display()))?;
    Ok(())
}
