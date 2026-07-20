//! Tray/menu-bar status for the watch daemon (Windows + macOS; Linux tray
//! needs the GTK stack and is deferred). The watch loop runs on a worker
//! thread; the main thread runs the OS event loop, forwards menu clicks as
//! WatchCommands, and mirrors watch status into the first menu item.
#![cfg(any(windows, target_os = "macos"))]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::WatchCommand;

/// 32×32 generated dot icon — no bundled assets needed.
fn icon() -> tray_icon::Icon {
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let dist = (dx * dx + dy * dy).sqrt();
            let i = (y * S + x) * 4;
            if dist < 14.0 {
                rgba[i..i + 4].copy_from_slice(&[70, 150, 240, 255]);
            }
            if dist < 6.0 {
                rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, S as u32, S as u32).expect("static icon")
}

#[allow(unused_assignments, unused_variables)]
pub fn tray_main(settle: Duration) -> Result<()> {
    let (control_tx, control_rx) = mpsc::channel::<WatchCommand>();
    let (status_tx, status_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        if let Err(err) = crate::watch_loop(settle, Some(control_rx), Some(status_tx)) {
            eprintln!("watch loop ended with error: {err:#}");
        }
        // Watch loop gone (Quit or error): the event loop below exits via
        // the Quit branch; on error the tray keeps showing the last status.
    });

    let menu = Menu::new();
    let status_item = MenuItem::new("starting…", false, None);
    let sync_item = MenuItem::new("Sync now", true, None);
    let pause_item = MenuItem::new("Pause", true, None);
    let quit_item = MenuItem::new("Quit YASGM", true, None);
    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &sync_item,
        &pause_item,
        &quit_item,
    ])?;

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::new().build();
    #[cfg(target_os = "macos")]
    {
        // Menu-bar app: no Dock icon.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let menu_events = MenuEvent::receiver();
    // Held for its Drop impl (removes the icon); never read again after the
    // Init branch sets it, which is expected and not a bug.
    let mut tray: Option<TrayIcon> = None;
    let mut paused = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(400));

        // macOS requires creating the tray icon after the event loop starts.
        if let Event::NewEvents(StartCause::Init) = event {
            tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_icon(icon())
                    .with_tooltip("YASGM — save game sync")
                    .build()
                    .expect("tray icon"),
            );
        }

        while let Ok(menu_event) = menu_events.try_recv() {
            if menu_event.id == *sync_item.id() {
                let _ = control_tx.send(WatchCommand::SyncNow);
            } else if menu_event.id == *pause_item.id() {
                paused = !paused;
                pause_item.set_text(if paused { "Resume" } else { "Pause" });
                let _ = control_tx.send(WatchCommand::TogglePause);
            } else if menu_event.id == *quit_item.id() {
                let _ = control_tx.send(WatchCommand::Quit);
                *control_flow = ControlFlow::Exit;
            }
        }

        while let Ok(line) = status_rx.try_recv() {
            status_item.set_text(line);
        }
    });
}
