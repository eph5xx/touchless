//! Menu bar icon with shortcuts to the main window.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{
    AppHandle, Emitter, Manager, include_image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

const TRAY_ID: &str = "touchless";

/// Whether the menu bar should show the recording icon.
static RECORDING: AtomicBool = AtomicBool::new(false);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Touchless", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "History", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Touchless", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &history, &settings, &separator, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(include_image!("icons/tray.png"))
        .icon_as_template(true)
        .tooltip("Touchless")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app, None),
            "history" => show_main(app, Some("history")),
            "settings" => show_main(app, Some("settings")),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Shows the red-headed jellyfish while dictation is in progress.
pub fn set_recording(app: &AppHandle, recording: bool) {
    if RECORDING.swap(recording, Ordering::Relaxed) == recording {
        return;
    }
    // Tray calls block until the main thread runs them; posting keeps the
    // hotkey thread and async tasks from waiting on it.
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = handle.tray_by_id(TRAY_ID) else {
            return;
        };
        // Read the flag here, not at post time, so out-of-order posts from
        // different threads still settle on the latest state.
        let recording = RECORDING.load(Ordering::Relaxed);
        let icon = if recording {
            include_image!("icons/recording.png")
        } else {
            include_image!("icons/tray.png")
        };
        // Template images keep only the alpha channel, which would drop the
        // red head, so the recording icon is drawn in its own colors instead.
        if let Err(error) = tray.set_icon_with_as_template(Some(icon), !recording) {
            eprintln!("could not update the tray icon: {error}");
        }
    });
}

/// Shows and focuses the main window, optionally switching to a page.
pub fn show_main(app: &AppHandle, page: Option<&str>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    if let Some(page) = page {
        let _ = app.emit_to("main", "navigate", page);
    }
}
