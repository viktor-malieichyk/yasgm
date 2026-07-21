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

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_plugin_shell::ShellExt;

/// Shows and focuses the main window (tray menu "Show", tray icon click,
/// and macOS Dock-icon "reopen" all funnel through here).
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

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

// ---- settings: cloud provider + autostart ----------------------------

/// `{"type": "onedrive"}` or `{"type": "local", "path": "..."}` (see
/// `provider_cmd` in main.rs).
#[tauri::command]
async fn get_provider(app: AppHandle) -> Result<serde_json::Value, String> {
    let stdout = run_yasgm(&app, &["provider", "--json"]).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

#[tauri::command]
async fn set_provider_onedrive(app: AppHandle) -> Result<String, String> {
    run_yasgm(&app, &["provider", "onedrive"]).await
}

#[tauri::command]
async fn set_provider_local(app: AppHandle, path: String) -> Result<String, String> {
    run_yasgm(&app, &["provider", "local", &path]).await
}

/// Runs the real connectivity check (Graph app-folder probe for OneDrive,
/// directory read/write probe for LocalFolder).
#[tauri::command]
async fn cloud_status(app: AppHandle) -> Result<String, String> {
    run_yasgm(&app, &["status"]).await
}

/// Interactive OneDrive sign-in: opens the system browser and blocks until
/// the user finishes (or the flow fails/expires). No-op message if the
/// provider is currently LocalFolder.
#[tauri::command]
async fn cloud_auth(app: AppHandle) -> Result<String, String> {
    run_yasgm(&app, &["auth"]).await
}

/// `{"enabled": bool, "detail": string|null}` (see `autostart_cmd` in
/// main.rs).
#[tauri::command]
async fn get_autostart(app: AppHandle) -> Result<serde_json::Value, String> {
    let stdout = run_yasgm(&app, &["autostart", "--json"]).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

#[tauri::command]
async fn set_autostart(app: AppHandle, enabled: bool) -> Result<String, String> {
    run_yasgm(&app, &["autostart", if enabled { "on" } else { "off" }]).await
}

/// Closing the window hides it instead of quitting (minimize to tray); the
/// tray icon's "Show YASGM" / left-click, or clicking the Dock icon again
/// on macOS, bring it back. "Quit YASGM" in the tray menu is the only way
/// to actually exit.
///
/// Single-instance (must be the first plugin registered, per its docs): a
/// second launch attempt hands its argv/cwd to the already-running
/// instance via this callback instead of starting a second process, and we
/// just surface the existing window — a background daemon's tray icon
/// makes a stray duplicate process easy to end up with otherwise.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            list_versions,
            restore_version,
            list_games,
            set_game_config,
            clear_game_config,
            set_pinned,
            remove_version,
            get_provider,
            set_provider_onedrive,
            set_provider_local,
            cloud_status,
            cloud_auth,
            get_autostart,
            set_autostart
        ])
        .setup(|app| {
            let show_item = MenuItem::with_id(app, "show", "Show YASGM", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit YASGM", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("app icon for tray"))
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if let RunEvent::Reopen { .. } = event {
            show_main_window(app_handle);
        }
    });
}
