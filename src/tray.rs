//! Menu bar icon with shortcuts to the main window.

use tauri::{
    AppHandle, Emitter, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Touchless", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "History", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Touchless", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &history, &settings, &separator, &quit])?;

    TrayIconBuilder::with_id("touchless")
        .icon(tauri::include_image!("icons/tray.png"))
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
