mod db;
mod elevenlabs;
mod hotkey;
mod microphone;
mod openai;
mod overlay;
mod paste;
mod permissions;
mod plugin_api;
mod plugin_host;
mod plugins;
mod recorder;
mod settings;
mod shortcut;
mod tray;
mod updater;

use std::sync::Arc;

use db::{Db, Transcript};
use hotkey::Hotkey;
use overlay::Overlay;
use permissions::{Permissions, PrivacyPane};
use plugin_host::{PluginHost, PluginView};
use recorder::{Recorder, Snapshot};
use settings::{Preferences, Provider, Settings, SettingsView, TranscriptionOptions};
use shortcut::Shortcut;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use updater::{UpdateView, Updater};

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
    app: AppHandle,
    settings: State<'_, Settings>,
    provider: Provider,
    key: String,
) -> Result<SettingsView, String> {
    settings.set_api_key(provider, &key)?;
    let _ = app.emit("plugins-changed", ());
    Ok(settings.view())
}

#[tauri::command]
fn remove_api_key(
    app: AppHandle,
    settings: State<'_, Settings>,
    provider: Provider,
) -> Result<SettingsView, String> {
    settings.remove_api_key(provider)?;
    let _ = app.emit("plugins-changed", ());
    Ok(settings.view())
}

#[tauri::command]
async fn test_api_key(settings: State<'_, Settings>, provider: Provider) -> Result<String, String> {
    let key = settings
        .api_key(provider)
        .ok_or_else(|| "Add a key first".to_owned())?;
    match provider {
        Provider::ElevenLabs => settings::test_api_key(&key, &settings.transcription()).await,
        Provider::OpenAi => openai::OpenAi::new(key)?.verify().await,
    }
}

#[tauri::command]
fn list_plugins(host: State<'_, PluginHost>, settings: State<'_, Settings>) -> Vec<PluginView> {
    host.view(&plugin_host::available(&settings))
}

#[tauri::command]
fn set_plugin_enabled(
    db: State<'_, Db>,
    host: State<'_, PluginHost>,
    settings: State<'_, Settings>,
    id: String,
    enabled: bool,
) -> Result<Vec<PluginView>, String> {
    host.set_enabled(&db, &id, enabled)?;
    Ok(host.view(&plugin_host::available(&settings)))
}

#[tauri::command]
fn update_transcription(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    options: TranscriptionOptions,
) -> Result<SettingsView, String> {
    settings.set_transcription(&db, options)?;
    Ok(settings.view())
}

#[tauri::command]
fn update_preferences(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    preferences: Preferences,
) -> Result<SettingsView, String> {
    settings.set_preferences(&db, preferences)?;
    let _ = app.emit("preferences-changed", settings.preferences());
    Ok(settings.view())
}

// The shortcut commands are async so they run off the main thread: registering
// a combo waits for the main thread, which would deadlock with a sync command.
#[tauri::command]
async fn capture_shortcut(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    hotkey: State<'_, Hotkey>,
    recorder: State<'_, Arc<Recorder>>,
) -> Result<Option<SettingsView>, String> {
    match hotkey.capture().await {
        Ok(Some(shortcut)) => {
            change_shortcut(&db, &settings, &hotkey, &recorder, shortcut).map(Some)
        }
        other => {
            restore_shortcut(&settings, &hotkey, &recorder);
            other.map(|_| None)
        }
    }
}

#[tauri::command]
async fn cancel_shortcut_capture(hotkey: State<'_, Hotkey>) -> Result<(), String> {
    hotkey.cancel_capture();
    Ok(())
}

#[tauri::command]
async fn reset_shortcut(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    hotkey: State<'_, Hotkey>,
    recorder: State<'_, Arc<Recorder>>,
) -> Result<SettingsView, String> {
    change_shortcut(&db, &settings, &hotkey, &recorder, Shortcut::default())
}

/// Switches to `shortcut` and saves it; on failure the saved shortcut stays active.
fn change_shortcut(
    db: &Db,
    settings: &Settings,
    hotkey: &Hotkey,
    recorder: &Recorder,
    shortcut: Shortcut,
) -> Result<SettingsView, String> {
    let result = shortcut.normalized().and_then(|shortcut| {
        hotkey.apply(&shortcut)?;
        settings.set_shortcut(db, shortcut)
    });
    if result.is_err() {
        restore_shortcut(settings, hotkey, recorder);
    } else {
        recorder.set_hotkey(hotkey.status());
    }
    result.map(|()| settings.view())
}

fn restore_shortcut(settings: &Settings, hotkey: &Hotkey, recorder: &Recorder) {
    if let Err(error) = hotkey.apply(&settings.shortcut()) {
        eprintln!("could not restore the shortcut: {error}");
    }
    recorder.set_hotkey(hotkey.status());
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

#[tauri::command]
fn get_update_state(updater: State<'_, Arc<Updater>>) -> UpdateView {
    updater.view()
}

#[tauri::command]
async fn check_for_updates(updater: State<'_, Arc<Updater>>) -> Result<UpdateView, String> {
    updater.check(true).await;
    Ok(updater.view())
}

#[tauri::command]
fn install_update(updater: State<'_, Arc<Updater>>) -> Result<UpdateView, String> {
    updater.install()?;
    Ok(updater.view())
}

#[tauri::command]
fn open_release_page(updater: State<'_, Arc<Updater>>) -> Result<(), String> {
    updater.open_release_page()
}

fn main() {
    // The WebSocket client uses rustls; pin its crypto backend.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_state,
            toggle_recording,
            list_transcripts,
            delete_transcript,
            get_settings,
            set_api_key,
            remove_api_key,
            test_api_key,
            list_plugins,
            set_plugin_enabled,
            update_transcription,
            update_preferences,
            capture_shortcut,
            cancel_shortcut_capture,
            reset_shortcut,
            get_permissions,
            request_accessibility,
            open_privacy_settings,
            restart_app,
            get_update_state,
            check_for_updates,
            install_update,
            open_release_page
        ])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Db::open(&data_dir.join("touchless.db"))?;
            let settings = Settings::load(&db);
            let shortcut = settings.shortcut();
            app.manage(settings);
            app.manage(PluginHost::load(&db, plugins::all()));
            app.manage(db);

            let overlay = Overlay::new(app.handle());
            let recorder = Arc::new(Recorder::new(app.handle().clone(), overlay));
            app.manage(Arc::clone(&recorder));

            let hotkey_recorder = Arc::clone(&recorder);
            let hotkey = Hotkey::start(app.handle(), shortcut, move || hotkey_recorder.toggle());
            recorder.set_hotkey(hotkey.status());
            app.manage(hotkey);

            let updater = Arc::new(Updater::new(app.handle().clone()));
            app.manage(Arc::clone(&updater));
            updater::start_background_checks(updater);

            tray::create(app.handle())?;
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
        if let tauri::RunEvent::Reopen { .. } = _event {
            tray::show_main(_app, None);
        }
    });
}
