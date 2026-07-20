//! Minimal Tauri v2 shell validating the GUI framework lean from
//! DESIGN.md's "UI plan": a version browser + restore button driving the
//! real `yasgm` CLI/Store, not mock data.
//!
//! Talks to `yasgm` as a subprocess (its `versions --json` output, its
//! process exit code for `restore`) rather than linking the core crate as a
//! library — the main crate isn't split into lib+bin yet. That refactor is
//! the natural next step once this framework choice is confirmed; for now
//! it keeps this shell decoupled while still exercising the real Store.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locates the `yasgm` binary: prefers the workspace's own dev build (this
/// crate lives at `<repo>/ui/src-tauri`, `yasgm` builds to
/// `<repo>/target/debug`) and falls back to PATH for a packaged/installed
/// binary.
fn yasgm_path() -> PathBuf {
    let exe_name = if cfg!(windows) { "yasgm.exe" } else { "yasgm" };
    let dev_build = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(exe_name);
    if dev_build.exists() { dev_build } else { PathBuf::from(exe_name) }
}

fn run_yasgm(args: &[&str]) -> Result<String, String> {
    let output = Command::new(yasgm_path())
        .args(args)
        .output()
        .map_err(|err| format!("failed to run yasgm: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One row from `yasgm versions --json` (see `versions_cmd` in main.rs).
#[tauri::command]
fn list_versions(app_id: Option<u64>) -> Result<Vec<serde_json::Value>, String> {
    let id_arg = app_id.map(|id| id.to_string());
    let mut args = vec!["versions"];
    if let Some(id) = &id_arg {
        args.push(id);
    }
    args.push("--json");
    let stdout = run_yasgm(&args)?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

#[tauri::command]
fn restore_version(app_id: u64, version_id: String) -> Result<String, String> {
    let app_id = app_id.to_string();
    run_yasgm(&["restore", &app_id, "--version", &version_id])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![list_versions, restore_version])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
