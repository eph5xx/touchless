mod chatgpt;
mod db;
mod elevenlabs;
mod external;
mod hotkey;
mod mcp_clients;
mod microphone;
mod models;
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
mod sound;
mod tips;
mod tray;
mod whisper;

use std::{path::PathBuf, sync::Arc};

use chatgpt::ChatGpt;
use db::{Db, HistoryPage, HistoryQuery};
use external::ExternalModelView;
use hotkey::Hotkey;
use mcp_clients::{ClientId, McpView};
use models::{LocalModel, ModelStore, ModelView, WhisperModel};
use overlay::Overlay;
use permissions::{Permissions, PrivacyPane};
use plugin_host::{PluginHost, PluginView};
use recorder::{Recorder, Snapshot};
use serde::Serialize;
use settings::{
    Engine, OpenAiMethod, Preferences, Provider, Settings, SettingsView, TranscriptionOptions,
};
use shortcut::Shortcut;
use sound::Sounds;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tips::Tips;
use whisper::{LocalWhisper, WhisperStatus};

#[tauri::command]
fn get_state(recorder: State<'_, Arc<Recorder>>) -> Snapshot {
    recorder.snapshot()
}

#[tauri::command]
fn toggle_recording(recorder: State<'_, Arc<Recorder>>) {
    recorder.toggle();
}

// Async so a search across a large history runs off the main thread.
#[tauri::command]
async fn list_transcripts(db: State<'_, Db>, query: HistoryQuery) -> Result<HistoryPage, String> {
    db.list_page(&query).map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_transcript(app: AppHandle, db: State<'_, Db>, id: i64) -> Result<(), String> {
    db.delete(id).map_err(|error| error.to_string())?;
    tray::transcripts_changed(&app);
    Ok(())
}

#[tauri::command]
fn fit_tips(tips: State<'_, Tips>, width: f64, height: f64) {
    tips.fit(width, height);
}

#[tauri::command]
fn open_transcript(app: AppHandle, id: i64) {
    tray::show_main(&app, Some("history"));
    let _ = app.emit_to("main", "open-transcript", id);
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
        Provider::OpenAi => {
            openai::OpenAi::new(openai::Auth::ApiKey(key))?
                .verify()
                .await
        }
    }
}

#[tauri::command]
fn set_openai_method(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    method: OpenAiMethod,
) -> Result<SettingsView, String> {
    settings.set_openai_method(&db, method)?;
    let _ = app.emit("plugins-changed", ());
    Ok(settings.view())
}

/// Waits for the browser sign-in; a successful one selects the ChatGPT subscription.
#[tauri::command]
async fn chatgpt_sign_in(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    chatgpt: State<'_, Arc<ChatGpt>>,
) -> Result<SettingsView, String> {
    let result = chatgpt.sign_in(&db).await;
    if result.as_ref().err().map(String::as_str) != Some(chatgpt::CANCELLED) {
        tray::show_main(&app, None);
    }
    result?;
    settings.set_openai_method(&db, OpenAiMethod::ChatGpt)?;
    let _ = app.emit("plugins-changed", ());
    Ok(settings.view())
}

#[tauri::command]
fn cancel_chatgpt_sign_in(chatgpt: State<'_, Arc<ChatGpt>>) {
    chatgpt.cancel_sign_in();
}

#[derive(Serialize)]
struct SignOut {
    settings: SettingsView,
    /// Whether OpenAI confirmed the session ended.
    revoked: bool,
}

#[tauri::command]
async fn chatgpt_sign_out(
    app: AppHandle,
    settings: State<'_, Settings>,
    chatgpt: State<'_, Arc<ChatGpt>>,
) -> Result<SignOut, String> {
    let revoked = chatgpt.sign_out().await?;
    let _ = app.emit("plugins-changed", ());
    Ok(SignOut {
        settings: settings.view(),
        revoked,
    })
}

#[tauri::command]
async fn test_chatgpt(chatgpt: State<'_, Arc<ChatGpt>>) -> Result<String, String> {
    if !chatgpt.signed_in() {
        return Err(chatgpt::NOT_SIGNED_IN.into());
    }
    openai::OpenAi::new(openai::Auth::ChatGpt(Arc::clone(&chatgpt)))?
        .verify()
        .await
}

#[tauri::command]
fn open_chatgpt_usage() -> Result<(), String> {
    chatgpt::open_url(chatgpt::USAGE_URL)
}

#[tauri::command]
fn dismiss_chatgpt_welcome(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    chatgpt: State<'_, Arc<ChatGpt>>,
) -> Result<SettingsView, String> {
    chatgpt.dismiss_welcome(&db)?;
    Ok(settings.view())
}

#[tauri::command]
fn list_models(models: State<'_, ModelStore>) -> Vec<ModelView> {
    models.view()
}

#[tauri::command]
fn get_whisper_status(whisper: State<'_, Arc<LocalWhisper>>) -> WhisperStatus {
    whisper.status()
}

#[tauri::command]
fn download_model(
    app: AppHandle,
    models: State<'_, ModelStore>,
    model: WhisperModel,
) -> Result<Vec<ModelView>, String> {
    models.download(&app, model, move |app| {
        // A model selected before its file was there loads once it arrives.
        let local = LocalModel::Builtin(model);
        if app.state::<Settings>().engine().local() == Some(local.clone()) {
            let path = app.state::<ModelStore>().path(model);
            app.state::<Arc<LocalWhisper>>().load(local, path);
        }
    })?;
    Ok(models.view())
}

#[tauri::command]
fn cancel_model_download(models: State<'_, ModelStore>, model: WhisperModel) {
    models.cancel(model);
}

/// Deleting the model in use switches back to ElevenLabs.
#[tauri::command]
fn delete_model(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    models: State<'_, ModelStore>,
    whisper: State<'_, Arc<LocalWhisper>>,
    model: WhisperModel,
) -> Result<SettingsView, String> {
    if settings.engine().local() == Some(LocalModel::Builtin(model)) {
        settings.set_engine(&db, Engine::Elevenlabs, &models)?;
        whisper.unload();
    }
    let result = models.delete(model);
    models.changed(&app);
    result.map(|()| settings.view())
}

#[tauri::command]
fn set_engine(
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    models: State<'_, ModelStore>,
    whisper: State<'_, Arc<LocalWhisper>>,
    engine: Engine,
) -> Result<SettingsView, String> {
    apply_engine(&db, &settings, &models, &whisper, engine)?;
    Ok(settings.view())
}

/// Saves `engine` and loads its model, or unloads Whisper for ElevenLabs.
fn apply_engine(
    db: &Db,
    settings: &Settings,
    models: &ModelStore,
    whisper: &Arc<LocalWhisper>,
    engine: Engine,
) -> Result<(), String> {
    let changed = settings.engine() != engine;
    settings.set_engine(db, engine.clone(), models)?;
    match engine.local() {
        Some(model) if changed || !matches!(whisper.status(), WhisperStatus::Ready { .. }) => {
            let path = models.local_path(&model);
            whisper.load(model, path);
        }
        Some(_) => {}
        None => whisper.unload(),
    }
    Ok(())
}

#[tauri::command]
fn list_external_models(settings: State<'_, Settings>) -> Vec<ExternalModelView> {
    external::views(&settings.external_models())
}

fn external_models_changed(app: &AppHandle, settings: &Settings) {
    let _ = app.emit(
        "external-models-changed",
        external::views(&settings.external_models()),
    );
}

/// Lets the user pick a whisper.cpp file, then adds and selects it.
/// Returns `None` when the user cancels.
#[tauri::command]
async fn add_external_model(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    models: State<'_, ModelStore>,
    whisper: State<'_, Arc<LocalWhisper>>,
) -> Result<Option<SettingsView>, String> {
    let (picked, chosen) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose a whisper.cpp model")
        .add_filter("whisper.cpp model", &["bin"]);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    if let Some(folder) = external::suggested_folder() {
        dialog = dialog.set_directory(folder);
    }
    dialog.pick_file(move |file| {
        let _ = picked.send(file);
    });
    let Some(file) = chosen.await.ok().flatten() else {
        return Ok(None);
    };
    let path = file.into_path().map_err(|error| error.to_string())?;

    external::inspect(&path)?;
    settings.add_external_model(&db, path.clone())?;
    external_models_changed(&app, &settings);
    apply_engine(&db, &settings, &models, &whisper, Engine::External(path))?;
    Ok(Some(settings.view()))
}

/// Forgets a picked model file without touching the file. Removing the
/// model in use switches back to ElevenLabs.
#[tauri::command]
fn remove_external_model(
    app: AppHandle,
    db: State<'_, Db>,
    settings: State<'_, Settings>,
    models: State<'_, ModelStore>,
    whisper: State<'_, Arc<LocalWhisper>>,
    path: PathBuf,
) -> Result<SettingsView, String> {
    if settings.engine() == Engine::External(path.clone()) {
        settings.set_engine(&db, Engine::Elevenlabs, &models)?;
        whisper.unload();
    }
    settings.remove_external_model(&db, &path)?;
    external_models_changed(&app, &settings);
    Ok(settings.view())
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
    tips: State<'_, Tips>,
    id: String,
    enabled: bool,
) -> Result<Vec<PluginView>, String> {
    if host.set_enabled(&db, &id, enabled)?
        && let Some((name, card, duration)) = host.example(&id)
    {
        let hint = format!("{name} tips will appear here");
        tips.show_example(&card, duration, &hint);
    }
    Ok(host.view(&plugin_host::available(&settings)))
}

#[tauri::command]
fn set_plugin_setting(
    db: State<'_, Db>,
    host: State<'_, PluginHost>,
    settings: State<'_, Settings>,
    id: String,
    setting: String,
    value: serde_json::Value,
) -> Result<Vec<PluginView>, String> {
    host.set_setting(&db, &id, &setting, &value)?;
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

fn mcp_server_path() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let dir = exe.parent().ok_or("Could not find the Touchless folder")?;
    Ok(dir.join("touchless-mcp"))
}

#[tauri::command]
async fn list_mcp_clients() -> Result<McpView, String> {
    let env = mcp_clients::Env::detect().await?;
    Ok(mcp_clients::view(&env, &mcp_server_path()?))
}

#[tauri::command]
async fn connect_mcp_client(id: ClientId) -> Result<McpView, String> {
    let server = mcp_server_path()?;
    if !server.is_file() {
        return Err(format!("touchless-mcp is missing at {}", server.display()));
    }
    let env = mcp_clients::Env::detect().await?;
    mcp_clients::connect(&env, id, &server).await?;
    Ok(mcp_clients::view(&env, &server))
}

#[tauri::command]
async fn disconnect_mcp_client(id: ClientId) -> Result<McpView, String> {
    let server = mcp_server_path()?;
    let env = mcp_clients::Env::detect().await?;
    mcp_clients::disconnect(&env, id).await?;
    Ok(mcp_clients::view(&env, &server))
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

fn main() {
    // The WebSocket client uses rustls; pin its crypto backend.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_state,
            toggle_recording,
            list_transcripts,
            delete_transcript,
            fit_tips,
            open_transcript,
            get_settings,
            set_api_key,
            remove_api_key,
            test_api_key,
            set_openai_method,
            chatgpt_sign_in,
            cancel_chatgpt_sign_in,
            chatgpt_sign_out,
            test_chatgpt,
            open_chatgpt_usage,
            dismiss_chatgpt_welcome,
            list_models,
            get_whisper_status,
            download_model,
            cancel_model_download,
            delete_model,
            set_engine,
            list_external_models,
            add_external_model,
            remove_external_model,
            list_plugins,
            set_plugin_enabled,
            set_plugin_setting,
            update_transcription,
            update_preferences,
            capture_shortcut,
            cancel_shortcut_capture,
            reset_shortcut,
            get_permissions,
            request_accessibility,
            open_privacy_settings,
            list_mcp_clients,
            connect_mcp_client,
            disconnect_mcp_client,
            restart_app
        ])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Db::open(&data_dir.join("touchless.db"))?;
            let chatgpt = Arc::new(ChatGpt::load(&db));
            let handle = app.handle().clone();
            chatgpt.on_lost(move || {
                let _ = handle.emit("plugins-changed", ());
            });
            let settings = Settings::load(&db, Arc::clone(&chatgpt));
            let shortcut = settings.shortcut();

            let models = ModelStore::new(data_dir.join("models"));
            let whisper = LocalWhisper::new(app.handle().clone());
            let available = |model: &LocalModel| match model {
                LocalModel::Builtin(model) => models.is_downloaded(*model),
                LocalModel::External(path) => path.is_file(),
            };
            if let Some(model) = settings.engine().local()
                && available(&model)
            {
                let path = models.local_path(&model);
                whisper.load(model, path);
            }
            app.manage(models);
            app.manage(whisper);
            app.manage(settings);
            app.manage(chatgpt);
            app.manage(PluginHost::load(&db, plugins::all()));
            app.manage(db);

            let overlay = Overlay::new(app.handle());
            app.manage(Tips::new(app.handle()));
            let sounds = Sounds::new(app.handle());
            let recorder = Arc::new(Recorder::new(app.handle().clone(), overlay, sounds));
            app.manage(Arc::clone(&recorder));

            let hotkey_recorder = Arc::clone(&recorder);
            let hotkey = Hotkey::start(app.handle(), shortcut, move || hotkey_recorder.toggle());
            recorder.set_hotkey(hotkey.status());
            app.manage(hotkey);

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
