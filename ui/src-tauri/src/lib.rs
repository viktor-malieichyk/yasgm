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

use std::collections::HashMap;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
#[cfg(target_os = "macos")]
use tauri::RunEvent;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::{Update, UpdaterExt};

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

/// Games with cloud saves that aren't installed here (`yasgm cloud-games
/// --json`); `cached` returns the last result instantly instead of asking
/// the cloud (a few seconds).
#[tauri::command]
async fn cloud_games(app: AppHandle, cached: bool) -> Result<Vec<serde_json::Value>, String> {
    let args: &[&str] = if cached { &["cloud-games", "--cached"] } else { &["cloud-games", "--json"] };
    let stdout = run_yasgm(&app, args).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

/// appid → `data:` URL of the game's icon, from `yasgm icons --json --embed`
/// (Steam's local image cache, plus a one-time download for the rest unless
/// `offline`). Games without any icon are left out.
#[tauri::command]
async fn game_icons(app: AppHandle, offline: bool) -> Result<HashMap<String, String>, String> {
    let mut args = vec!["icons", "--json", "--embed"];
    if offline {
        args.push("--offline");
    }
    let stdout = run_yasgm(&app, &args).await?;
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))?;
    Ok(rows
        .iter()
        .filter_map(|row| Some((row["app_id"].as_u64()?.to_string(), row["data_url"].as_str()?.to_owned())))
        .collect())
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

// ---- open in Finder/Explorer -------------------------------------------

// The page asks to open a game's location; the path or URL is looked up
// here, never taken from the page. With a generic "open this path" command,
// injected markup could open (and so run) any file.

fn json_out(stdout: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

/// Opens the game's first local save folder (`yasgm paths <appid>`: only
/// ones that exist). `false` if it has none on this machine.
#[tauri::command]
async fn open_save_location(app: AppHandle, app_id: u64) -> Result<bool, String> {
    let paths = json_out(&run_yasgm(&app, &["paths", &app_id.to_string()]).await?)?;
    let Some(path) = paths.get(0).and_then(|p| p["path"].as_str()) else {
        return Ok(false);
    };
    app.opener().open_path(path, None::<&str>).map_err(|err| err.to_string())?;
    Ok(true)
}

/// Opens where this game's backups live on the primary provider (D8): the
/// OneDrive web page of its folder, or the local folder (`yasgm
/// backup-location`). `false` if OneDrive gave no link.
#[tauri::command]
async fn open_backup_location(app: AppHandle, app_id: u64) -> Result<bool, String> {
    let loc = json_out(&run_yasgm(&app, &["backup-location", &app_id.to_string()]).await?)?;
    if loc["kind"] == "cloud" {
        let Some(url) = loc["url"].as_str() else {
            return Ok(false);
        };
        if !url.starts_with("https://") {
            return Err(format!("not opening an unexpected link: {url}"));
        }
        app.opener().open_url(url, None::<&str>).map_err(|err| err.to_string())?;
    } else {
        let path = loc["path"].as_str().ok_or("no folder in yasgm's answer")?;
        app.opener().open_path(path, None::<&str>).map_err(|err| err.to_string())?;
    }
    Ok(true)
}

/// Opens the local folder holding this game's offline (pending) backups
/// (D18; `yasgm pending-location`).
#[tauri::command]
async fn open_pending_location(app: AppHandle, app_id: u64) -> Result<(), String> {
    let loc = json_out(&run_yasgm(&app, &["pending-location", &app_id.to_string()]).await?)?;
    let path = loc["path"].as_str().ok_or("no folder in yasgm's answer")?;
    app.opener().open_path(path, None::<&str>).map_err(|err| err.to_string())
}

/// Best-effort approximation of the user's real macOS accent color (System
/// Settings > Appearance > Accent color). WebKit's CSS `AccentColor`/
/// `AccentColorText` system-color keywords were tried first but don't
/// reliably reflect it in this WKWebView — empirically they always render
/// macOS's own default blue regardless of the actual setting (verified: a
/// system set to Purple, `AppleAccentColor` = 5, still rendered blue). So
/// instead this reads the same `AppleAccentColor` NSUserDefaults key AppKit
/// itself reads and maps it against Apple's known, stable accent palette.
/// `text` is always white, matching how macOS itself renders
/// selection/accent-button text regardless of hue (not computed contrast).
#[tauri::command]
fn get_accent_color() -> serde_json::Value {
    #[cfg(target_os = "macos")]
    let accent = {
        let index: Option<i32> = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleAccentColor"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok());
        match index {
            Some(-1) => "#8e8e93", // Graphite
            Some(0) => "#ff3b30",  // Red
            Some(1) => "#ff9500",  // Orange
            Some(2) => "#ffcc00",  // Yellow
            Some(3) => "#34c759",  // Green
            Some(4) => "#007aff",  // Blue
            Some(5) => "#af52de",  // Purple
            Some(6) => "#ff2d55",  // Pink
            _ => "#0a84ff",        // Multicolor, or key unset: macOS's own default
        }
    };
    #[cfg(not(target_os = "macos"))]
    let accent = "#0a84ff";

    serde_json::json!({"accent": accent, "text": "#ffffff"})
}

// ---- settings: cloud provider + autostart ----------------------------

/// `{"type": "onedrive"}` or `{"type": "local", "path": "..."}` (see
/// `provider_cmd` in main.rs).
#[tauri::command]
async fn get_provider(app: AppHandle) -> Result<serde_json::Value, String> {
    let stdout = run_yasgm(&app, &["provider", "--json"]).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

/// `yasgm status --json`: provider, whether signed in, whether the cloud is
/// reachable (a real probe — can take a moment for OneDrive), daemon state.
#[tauri::command]
async fn get_cloud_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let stdout = run_yasgm(&app, &["status", "--json"]).await?;
    serde_json::from_str(stdout.trim()).map_err(|err| format!("parsing yasgm output: {err}"))
}

/// Signs out of OneDrive on this device / stops using the folder. Saves in
/// the cloud and the folder's contents are untouched.
#[tauri::command]
async fn disconnect_provider(app: AppHandle, sign_in: State<'_, DeviceSignIn>) -> Result<String, String> {
    sign_in.cancel();
    run_yasgm(&app, &["provider", "disconnect"]).await
}

/// The running `auth --device --json` child, if a QR sign-in is in progress.
#[derive(Default)]
struct DeviceSignIn(std::sync::Mutex<Option<CommandChild>>);

impl DeviceSignIn {
    fn cancel(&self) {
        if let Some(child) = self.0.lock().unwrap().take() {
            let _ = child.kill();
        }
    }
}

/// Starts the OneDrive device-code sign-in for a QR code. Each JSON line the
/// CLI prints is emitted to the page as an `auth-event`: `code` (with the
/// link and user code to show), `waiting`, then `signed_in` or `error`.
#[tauri::command]
fn start_device_sign_in(app: AppHandle, sign_in: State<'_, DeviceSignIn>) -> Result<(), String> {
    sign_in.cancel();
    let (mut events, child) = app
        .shell()
        .sidecar("yasgm")
        .map_err(|err| format!("failed to prepare yasgm sidecar: {err}"))?
        .args(["auth", "--device", "--json"])
        .spawn()
        .map_err(|err| format!("failed to run yasgm: {err}"))?;
    let pid = child.pid();
    *sign_in.0.lock().unwrap() = Some(child);

    tauri::async_runtime::spawn(async move {
        let mut buffer = String::new();
        let mut finished = false;
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(end) = buffer.find('\n') {
                        let line: String = buffer.drain(..=end).collect();
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
                        finished |= matches!(value["event"].as_str(), Some("signed_in" | "error" | "not_needed"));
                        let _ = app.emit("auth-event", value);
                    }
                }
                CommandEvent::Terminated(_) | CommandEvent::Error(_) => {
                    if !finished {
                        let _ = app.emit("auth-event", serde_json::json!({"event": "error", "message": "sign-in stopped"}));
                    }
                    break;
                }
                _ => {}
            }
        }
        let state = app.state::<DeviceSignIn>();
        let mut current = state.0.lock().unwrap();
        if current.as_ref().is_some_and(|c| c.pid() == pid) {
            *current = None;
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_device_sign_in(sign_in: State<'_, DeviceSignIn>) {
    sign_in.cancel();
}

#[tauri::command]
async fn set_provider_local(app: AppHandle, path: String) -> Result<String, String> {
    run_yasgm(&app, &["provider", "local", &path]).await
}

/// Native folder picker for the LocalFolder path field. `None` if the user
/// cancelled. The dialog plugin's API is callback-based (it has to run on
/// the main thread on macOS), so this bridges it into the async command
/// with a plain channel; blocking the task while the modal is open is fine
/// since that's the whole point of asking the user to pick something.
#[tauri::command]
async fn pick_local_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    rx.recv().ok().flatten().map(|f| f.to_string())
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

// ---- auto-update (D20) ----------------------------------------------------

/// The update found by the last `check_update`, kept so `install_update`
/// installs exactly what the user was shown.
#[derive(Default)]
struct PendingUpdate(std::sync::Mutex<Option<Update>>);

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// `null` when up to date, else `{"version", "current", "notes", "date"}`.
/// The feed is `latest.json` on GitHub Releases (tauri.conf.json →
/// plugins.updater.endpoints); `YASGM_UPDATE_ENDPOINT` overrides it for
/// local testing. The override can't weaken anything: artifacts are still
/// verified against the baked-in public key.
#[tauri::command]
async fn check_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> Result<Option<serde_json::Value>, String> {
    let mut builder = app.updater_builder();
    if let Ok(endpoint) = std::env::var("YASGM_UPDATE_ENDPOINT") {
        let url = endpoint.parse().map_err(|err| format!("YASGM_UPDATE_ENDPOINT: {err}"))?;
        builder = builder.endpoints(vec![url]).map_err(|err| err.to_string())?;
    }
    let update = builder
        .build()
        .map_err(|err| err.to_string())?
        .check()
        .await
        .map_err(|err| err.to_string())?;
    let info = update.as_ref().map(|u| {
        serde_json::json!({
            "version": u.version,
            "current": u.current_version,
            "notes": u.body,
            "date": u.date.map(|d| d.unix_timestamp()),
        })
    });
    *pending.0.lock().unwrap() = update;
    Ok(info)
}

/// Stops the watch daemon (its binary is the sidecar inside the bundle
/// being replaced — on Windows a running exe can't even be overwritten),
/// downloads + verifies + installs the update, then relaunches. The
/// relaunched GUI brings the daemon back via `ensure_daemon`. On Windows
/// the NSIS installer exits and restarts the app itself, so the restart
/// below is only reached on macOS/Linux.
#[tauri::command]
async fn install_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> Result<(), String> {
    let update = pending
        .0
        .lock()
        .unwrap()
        .take()
        .ok_or("no update pending — check for updates first")?;
    let stopped = run_yasgm(&app, &["watch", "stop", "--json"]).await?;
    let was_running = serde_json::from_str::<serde_json::Value>(stopped.trim())
        .ok()
        .and_then(|v| v["was_running"].as_bool())
        .unwrap_or(false);
    if let Err(err) = update.download_and_install(|_, _| {}, || {}).await {
        if was_running {
            let _ = run_yasgm(&app, &["watch", "start"]).await;
        }
        return Err(format!("update failed: {err}"));
    }
    app.restart();
}

/// If autostart is on, the user wants the daemon running: start it when it
/// isn't (no-op otherwise — the daemon lock makes this idempotent). This is
/// also what restores the daemon after `install_update` stopped it.
async fn ensure_daemon(app: AppHandle) {
    let enabled = run_yasgm(&app, &["autostart", "--json"])
        .await
        .ok()
        .and_then(|out| serde_json::from_str::<serde_json::Value>(out.trim()).ok())
        .and_then(|v| v["enabled"].as_bool())
        .unwrap_or(false);
    if enabled {
        if let Err(err) = run_yasgm(&app, &["watch", "start"]).await {
            eprintln!("could not start watch daemon: {err}");
        }
    }
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingUpdate::default())
        .manage(DeviceSignIn::default())
        .invoke_handler(tauri::generate_handler![
            list_versions,
            game_icons,
            restore_version,
            list_games,
            cloud_games,
            set_game_config,
            clear_game_config,
            set_pinned,
            remove_version,
            get_provider,
            get_cloud_status,
            disconnect_provider,
            start_device_sign_in,
            cancel_device_sign_in,
            set_provider_local,
            pick_local_folder,
            cloud_auth,
            get_autostart,
            set_autostart,
            open_save_location,
            open_backup_location,
            open_pending_location,
            get_accent_color,
            app_version,
            check_update,
            install_update
        ])
        .setup(|app| {
            let show_item = MenuItem::with_id(app, "show", "Show YASGM", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit YASGM", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let tray_icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))
                .expect("decoding bundled tray icon");
            TrayIconBuilder::new()
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            tauri::async_runtime::spawn(ensure_daemon(app.handle().clone()));
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

    app.run(|_app_handle, _event| {
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { .. } = _event {
            show_main_window(_app_handle);
        }
    });
}
