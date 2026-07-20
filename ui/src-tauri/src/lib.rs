//! Minimal Tauri v2 shell validating the GUI framework lean from
//! DESIGN.md's "UI plan": a version browser + restore button driving the
//! real `yasgm` CLI/Store, not mock data.
//!
//! Talks to `yasgm` as a sidecar subprocess (its `versions --json` output,
//! its process exit code for `restore`) rather than linking the core crate
//! as a library — deferred per DESIGN.md's "Lib+bin split" note. The
//! sidecar is built and copied into `binaries/` by
//! `../scripts/build-sidecar.sh` (wired as `beforeDevCommand`/
//! `beforeBuildCommand` in tauri.conf.json) and bundled into the packaged
//! app via `bundle.externalBin`, so this works identically in `tauri dev`
//! and in a built `.app` — no dev-path guessing or reliance on PATH.

use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;

async fn run_yasgm(app: &AppHandle, args: &[&str]) -> Result<String, String> {
    let sidecar = app
        .shell()
        .sidecar("yasgm")
        .map_err(|err| format!("failed to prepare yasgm sidecar: {err}"))?;
    let output = sidecar
        .args(args)
        .output()
        .await
        .map_err(|err| format!("failed to run yasgm: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One row from `yasgm versions --json` (see `versions_cmd` in main.rs).
#[tauri::command]
async fn list_versions(app: AppHandle, app_id: Option<u64>) -> Result<Vec<serde_json::Value>, String> {
    let id_arg = app_id.map(|id| id.to_string());
    let mut args = vec!["versions"];
    if let Some(id) = &id_arg {
        args.push(id);
    }
    args.push("--json");
    let stdout = run_yasgm(&app, &args).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

#[tauri::command]
async fn restore_version(app: AppHandle, app_id: u64, version_id: String) -> Result<String, String> {
    let app_id = app_id.to_string();
    run_yasgm(&app, &["restore", &app_id, "--version", &version_id]).await
}

/// One row from `yasgm config --json` (see `config_cmd` in main.rs).
#[tauri::command]
async fn list_games(app: AppHandle) -> Result<Vec<serde_json::Value>, String> {
    let stdout = run_yasgm(&app, &["config", "--json"]).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

#[tauri::command]
async fn set_game_config(
    app: AppHandle,
    app_id: u64,
    mode: String,
    keep: Option<u32>,
) -> Result<String, String> {
    let app_id = app_id.to_string();
    let mut args = vec!["config", &app_id, "--mode", &mode];
    let keep_str;
    if let Some(keep) = keep {
        keep_str = keep.to_string();
        args.push("--keep");
        args.push(&keep_str);
    }
    run_yasgm(&app, &args).await
}

#[tauri::command]
async fn clear_game_config(app: AppHandle, app_id: u64) -> Result<String, String> {
    let app_id = app_id.to_string();
    run_yasgm(&app, &["config", &app_id, "--clear"]).await
}

#[tauri::command]
async fn set_pinned(app: AppHandle, app_id: u64, version_id: String, pinned: bool) -> Result<String, String> {
    let app_id = app_id.to_string();
    run_yasgm(&app, &[if pinned { "pin" } else { "unpin" }, &app_id, &version_id]).await
}

#[tauri::command]
async fn remove_version(app: AppHandle, app_id: u64, version_id: String) -> Result<String, String> {
    let app_id = app_id.to_string();
    run_yasgm(&app, &["rm", &app_id, &version_id]).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            list_versions,
            restore_version,
            list_games,
            set_game_config,
            clear_game_config,
            set_pinned,
            remove_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
