mod db;
mod elevenlabs;
mod hotkey;
mod microphone;
mod recorder;

use std::sync::Arc;

use db::{Db, Transcript};
use recorder::{Recorder, Snapshot};
use tauri::{Manager, State, WindowEvent};

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

fn main() {
    dotenvy::dotenv().ok();

    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_state,
            toggle_recording,
            list_transcripts
        ])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            app.manage(Db::open(&data_dir.join("touchless.db"))?);

            let recorder = Arc::new(Recorder::new(app.handle().clone()));
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
