mod db;
mod elevenlabs;
mod hotkey;
mod microphone;
mod overlay;
mod paste;
mod permissions;
mod recorder;
mod settings;

use std::sync::Arc;

use db::{Db, Transcript};
use overlay::Overlay;
use permissions::{Permissions, PrivacyPane};
use recorder::{Recorder, Snapshot};
use settings::{Preferences, Provider, Settings, SettingsView};
use tauri::{AppHandle, Manager, State, WindowEvent};

#[tauri::command]
fn get_state(recorder: State<'_, Arc<Recorder>>) -> Snapshot {
    recorder.snapshot()
}

#[tauri::command]
fn toggle_recording(recorder: State<'_, Arc<Recorder>>) {
    recorder.toggle();
}

#[tauri::command]
fn list_transcripts(db: State<'_, Db>) -> Result<Vec<Transcript>, String> {
    db.list().map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_transcript(db: State<'_, Db>, id: i64) -> Result<(), String> {
    db.delete(id).map_err(|error| error.to_string())
}

#[tauri::command]
fn get_settings(settings: State<'_, Settings>) -> SettingsView {
    settings.view()
}

#[tauri::command]
fn set_api_key(
    settings: State<'_, Settings>,
    provider: Provider,
    key: String,
) -> Result<SettingsView, String> {
    settings.set_api_key(provider, &key)?;
    Ok(settings.view())
}

#[tauri::command]
fn remove_api_key(
    settings: State<'_, Settings>,
    provider: Provider,
) -> Result<SettingsView, String> {
    settings.remove_api_key(provider)?;
    Ok(settings.view())
}

#[tauri::command]
async fn test_api_key(settings: State<'_, Settings>, provider: Provider) -> Result<String, String> {
    let key = settings
        .api_key(provider)
        .ok_or_else(|| "Add a key first".to_owned())?;
    settings::test_api_key(provider, &key).await
}

#[tauri::command]
fn update_preferences(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    preferences: Preferences,
) -> Result<SettingsView, String> {
    settings.set_preferences(&db, preferences)?;
    Ok(settings.view())
}

#[tauri::command]
fn get_permissions() -> Permissions {
    permissions::current()
}

#[tauri::command]
fn request_accessibility() -> bool {
    permissions::request_accessibility()
}

#[tauri::command]
fn open_privacy_settings(pane: PrivacyPane) -> Result<(), String> {
    permissions::open_privacy_settings(pane)
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

fn main() {
    dotenvy::dotenv().ok();
    // Both the WebSocket and HTTPS clients use rustls; pin its crypto backend.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_state,
            toggle_recording,
            list_transcripts,
            delete_transcript,
            get_settings,
            set_api_key,
            remove_api_key,
            test_api_key,
            update_preferences,
            get_permissions,
            request_accessibility,
            open_privacy_settings,
            restart_app
        ])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Db::open(&data_dir.join("touchless.db"))?;
            app.manage(Settings::load(&db));
            app.manage(db);

            let overlay = Overlay::new(app.handle());
            let recorder = Arc::new(Recorder::new(app.handle().clone(), overlay));
            app.manage(Arc::clone(&recorder));

            let hotkey_recorder = Arc::clone(&recorder);
            recorder.set_hotkey(hotkey::spawn(move || hotkey_recorder.toggle()));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep running in the background so the hotkey keeps working.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Touchless");

    app.run(|_app, _event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event
            && let Some(window) = _app.get_webview_window("main")
        {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
}
