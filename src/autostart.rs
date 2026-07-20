//! Opt-in autostart of the watch daemon at login (D11).
//!
//! Windows: HKCU Run registry value. macOS: LaunchAgent plist (loaded
//! immediately, best-effort). Linux: XDG autostart .desktop entry (headless
//! watch — no Linux tray yet); written per spec, not yet validated on
//! hardware.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot determine own executable path")
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn plist_path() -> Result<PathBuf> {
        Ok(dirs::home_dir()
            .context("no home dir")?
            .join("Library/LaunchAgents/dev.yasgm.watch.plist"))
    }

    pub fn enable() -> Result<String> {
        let exe = exe()?;
        let path = plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.yasgm.watch</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>watch</string>
        <string>--tray</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
            exe.display()
        );
        std::fs::write(&path, plist)?;
        // Start it now too (best effort; the plist alone covers next login).
        let _ = std::process::Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&path)
            .output();
        Ok(format!("LaunchAgent installed: {}", path.display()))
    }

    pub fn disable() -> Result<String> {
        let path = plist_path()?;
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&path)
            .output();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok("LaunchAgent removed".to_owned())
    }

    pub fn status() -> Result<Option<String>> {
        let path = plist_path()?;
        Ok(path.exists().then(|| path.display().to_string()))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE: &str = "YASGM";

    pub fn enable() -> Result<String> {
        let exe = exe()?;
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_KEY)
            .context("opening HKCU Run key")?;
        let command = format!("\"{}\" watch --tray", exe.display());
        key.set_value(VALUE, &command).context("writing Run value")?;
        Ok(format!("registry Run entry set: {command}"))
    }

    pub fn disable() -> Result<String> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_ALL_ACCESS)
            .context("opening HKCU Run key")?;
        match key.delete_value(VALUE) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("removing Run value"),
        }
        Ok("registry Run entry removed".to_owned())
    }

    pub fn status() -> Result<Option<String>> {
        let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN_KEY) {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };
        Ok(key.get_value::<String, _>(VALUE).ok())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;

    fn desktop_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("no config dir")?
            .join("autostart/yasgm.desktop"))
    }

    pub fn enable() -> Result<String> {
        let exe = exe()?;
        let path = desktop_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=YASGM\nComment=Save game sync daemon\n\
             Exec=\"{}\" watch\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        std::fs::write(&path, entry)?;
        Ok(format!("autostart entry installed: {}", path.display()))
    }

    pub fn disable() -> Result<String> {
        let path = desktop_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok("autostart entry removed".to_owned())
    }

    pub fn status() -> Result<Option<String>> {
        let path = desktop_path()?;
        Ok(path.exists().then(|| path.display().to_string()))
    }
}

pub fn enable() -> Result<String> {
    imp::enable()
}

pub fn disable() -> Result<String> {
    imp::disable()
}

pub fn status() -> Result<Option<String>> {
    imp::status()
}
