//! Menu bar icon with shortcuts to the main window.

use std::sync::{Arc, Mutex};

use tauri::{
    AppHandle, Emitter, Manager, Wry, include_image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

use crate::{recorder::Status, updater::Updater};

const TRAY_ID: &str = "touchless";
const CHECK_FOR_UPDATES: &str = "Check for Updates…";

/// The menu item that checks for updates, or offers the one found.
struct UpdateItem {
    item: MenuItem<Wry>,
    label: Mutex<String>,
}

/// The latest icon requested, including changes waiting for the main thread.
static ICON_STATE: Mutex<IconState> = Mutex::new(IconState::Idle);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconState {
    Idle,
    Recording,
    Finalizing,
}

impl From<Status> for IconState {
    fn from(status: Status) -> Self {
        match status {
            Status::Idle => Self::Idle,
            Status::Connecting | Status::Listening => Self::Recording,
            Status::Finalizing => Self::Finalizing,
        }
    }
}

#[derive(Clone, Copy)]
enum IconInk {
    Black,
    White,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Touchless", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "History", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let updates = MenuItem::with_id(app, "updates", CHECK_FOR_UPDATES, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Touchless", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &history, &settings, &updates, &separator, &quit],
    )?;
    app.manage(UpdateItem {
        item: updates,
        label: Mutex::new(CHECK_FOR_UPDATES.into()),
    });

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
            "updates" => {
                show_main(app, Some("settings"));
                let updater = Arc::clone(&app.state::<Arc<Updater>>());
                tauri::async_runtime::spawn(async move { updater.check(true).await });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Names the update in the menu, or goes back to "Check for Updates…".
pub fn set_update_version(app: &AppHandle, version: Option<&str>) {
    let Some(updates) = app.try_state::<UpdateItem>() else {
        return;
    };
    let label = match version {
        Some(version) => format!("Update to v{version}…"),
        None => CHECK_FOR_UPDATES.into(),
    };
    {
        let mut current = updates.label.lock().unwrap();
        if *current == label {
            return;
        }
        *current = label;
    }
    // Posted like the icon change, so the caller never waits on the main thread.
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let updates = handle.state::<UpdateItem>();
        let label = updates.label.lock().unwrap().clone();
        if let Err(error) = updates.item.set_text(label) {
            eprintln!("could not update the tray menu: {error}");
        }
    });
}

/// Shows red while recording and yellow while finalizing the dictation.
pub fn set_status(app: &AppHandle, status: Status) {
    let state = IconState::from(status);
    {
        let mut current = ICON_STATE.lock().unwrap();
        if *current == state {
            return;
        }
        *current = state;
    }
    // Tray calls block until the main thread runs them; posting keeps the
    // hotkey thread and async tasks from waiting on it.
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = handle.tray_by_id(TRAY_ID) else {
            return;
        };
        // Read the state here, not at post time, so out-of-order posts from
        // different threads still settle on the latest state.
        let state = *ICON_STATE.lock().unwrap();
        let icon = match state {
            IconState::Idle => include_image!("icons/tray.png"),
            IconState::Recording => match icon_ink(&tray) {
                IconInk::Black => include_image!("icons/recording-black.png"),
                IconInk::White => include_image!("icons/recording.png"),
            },
            IconState::Finalizing => match icon_ink(&tray) {
                IconInk::Black => include_image!("icons/finalizing-black.png"),
                IconInk::White => include_image!("icons/finalizing.png"),
            },
        };
        // Template images keep only the alpha channel, which would drop the
        // colored head, so only the idle icon is drawn as a template.
        if let Err(error) = tray.set_icon_with_as_template(Some(icon), state == IconState::Idle) {
            eprintln!("could not update the tray icon: {error}");
        }
    });
}

/// Matches colored icons to the ink macOS uses for the template icon.
#[cfg(target_os = "macos")]
fn icon_ink(tray: &tauri::tray::TrayIcon) -> IconInk {
    use objc2_app_kit::NSAppearanceCustomization;
    use objc2_foundation::{MainThreadMarker, NSArray, NSString};

    tray.with_inner_tray_icon(|tray| {
        let Some(status_item) = tray.ns_status_item() else {
            return IconInk::White;
        };
        let Some(mtm) = MainThreadMarker::new() else {
            return IconInk::White;
        };
        let Some(button) = status_item.button(mtm) else {
            return IconInk::White;
        };

        let aqua = NSString::from_str("NSAppearanceNameAqua");
        let dark_aqua = NSString::from_str("NSAppearanceNameDarkAqua");
        let appearances = NSArray::from_slice(&[&*aqua, &*dark_aqua]);
        let is_dark = button
            .effectiveAppearance()
            .bestMatchFromAppearancesWithNames(&appearances)
            .is_some_and(|name| name.isEqualToString(&dark_aqua));
        if is_dark {
            IconInk::White
        } else {
            IconInk::Black
        }
    })
    .unwrap_or(IconInk::White)
}

#[cfg(not(target_os = "macos"))]
fn icon_ink(_tray: &tauri::tray::TrayIcon) -> IconInk {
    IconInk::White
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_recorder_status_to_icon_state() {
        for (status, expected) in [
            (Status::Idle, IconState::Idle),
            (Status::Connecting, IconState::Recording),
            (Status::Listening, IconState::Recording),
            (Status::Finalizing, IconState::Finalizing),
        ] {
            assert_eq!(IconState::from(status), expected, "{status:?}");
        }
    }
}
