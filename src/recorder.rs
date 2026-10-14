use std::{
    env,
    error::Error,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};

use crate::{
    db::Db,
    elevenlabs::{ElevenLabs, TranscriptEvent},
    hotkey::HotkeyStatus,
    microphone::Microphone,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Connecting,
    Listening,
    Finishing,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub status: Status,
    pub started_at: Option<i64>,
    pub committed: String,
    pub partial: String,
    pub error: Option<String>,
    pub hotkey: HotkeyStatus,
}

struct Inner {
    snapshot: Snapshot,
    stop: Option<std_mpsc::Sender<()>>,
}

pub struct Recorder {
    app: AppHandle,
    inner: Mutex<Inner>,
}

impl Recorder {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            inner: Mutex::new(Inner {
                snapshot: Snapshot {
                    status: Status::Idle,
                    started_at: None,
                    committed: String::new(),
                    partial: String::new(),
                    error: None,
                    hotkey: HotkeyStatus::Unavailable,
                },
                stop: None,
            }),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().snapshot.clone()
    }

    pub fn set_hotkey(&self, status: HotkeyStatus) {
        self.update(|snapshot| snapshot.hotkey = status);
    }

    pub fn toggle(self: &Arc<Self>) {
        let mut inner = self.inner.lock().unwrap();
        match inner.snapshot.status {
            Status::Idle => {
                let (stop_tx, stop_rx) = std_mpsc::channel();
                inner.stop = Some(stop_tx);
                let snapshot = &mut inner.snapshot;
                snapshot.status = Status::Connecting;
                snapshot.started_at = Some(now_ms());
                snapshot.committed.clear();
                snapshot.partial.clear();
                snapshot.error = None;
                let snapshot = snapshot.clone();
                drop(inner);
                self.emit(&snapshot);

                let recorder = Arc::clone(self);
                tauri::async_runtime::spawn(async move { recorder.session(stop_rx).await });
            }
            Status::Connecting | Status::Listening => {
                if let Some(stop) = inner.stop.take() {
                    let _ = stop.send(());
                }
                inner.snapshot.status = Status::Finishing;
                let snapshot = inner.snapshot.clone();
                drop(inner);
                self.emit(&snapshot);
            }
            Status::Finishing => {}
        }
    }

    async fn session(self: Arc<Self>, stop: std_mpsc::Receiver<()>) {
        let result = self.record(stop).await;

        let (text, started_at) = {
            let mut inner = self.inner.lock().unwrap();
            // Dropping the sender also stops the microphone thread on error paths.
            inner.stop = None;
            (
                inner.snapshot.committed.trim().to_owned(),
                inner.snapshot.started_at.unwrap_or_else(now_ms),
            )
        };

        let mut error = result.err().map(|error| error.to_string());
        if !text.is_empty() {
            let db = self.app.state::<Db>();
            match db.insert(&text, started_at, now_ms() - started_at) {
                Ok(_) => {
                    let _ = self.app.emit("transcripts-changed", ());
                }
                Err(db_error) => error = Some(format!("could not save transcript: {db_error}")),
            }
        }

        self.update(|snapshot| {
            snapshot.status = Status::Idle;
            snapshot.partial.clear();
            snapshot.error = error;
        });
    }

    async fn record(self: &Arc<Self>, stop: std_mpsc::Receiver<()>) -> Result<()> {
        let api_key = env::var("ELEVENLABS_API_KEY")
            .map_err(|_| "set ELEVENLABS_API_KEY in the environment or in .env")?;

        let (audio_tx, audio_rx) = mpsc::channel(32);
        let (rate_tx, rate_rx) = oneshot::channel();

        // cpal streams are not guaranteed to be Send, so the stream lives on its own
        // thread until the stop signal arrives (or the sender is dropped).
        std::thread::Builder::new()
            .name("microphone".into())
            .spawn(move || {
                let microphone = Microphone::open(audio_tx).and_then(|microphone| {
                    microphone.start()?;
                    Ok(microphone)
                });
                match microphone {
                    Ok(microphone) => {
                        let _ = rate_tx.send(Ok(microphone.sample_rate()));
                        let _ = stop.recv();
                    }
                    Err(error) => {
                        let _ = rate_tx.send(Err(error.to_string()));
                    }
                }
            })?;

        let sample_rate = rate_rx
            .await
            .map_err(|_| "microphone thread exited unexpectedly")??;
        let client = ElevenLabs::connect(&api_key, sample_rate).await?;

        self.update(|snapshot| {
            if snapshot.status == Status::Connecting {
                snapshot.status = Status::Listening;
            }
        });

        client
            .run(audio_rx, |event| {
                self.update(|snapshot| match event {
                    TranscriptEvent::Partial(text) => snapshot.partial = text,
                    TranscriptEvent::Committed(text) => {
                        let text = text.trim();
                        if !text.is_empty() {
                            if !snapshot.committed.is_empty() {
                                snapshot.committed.push(' ');
                            }
                            snapshot.committed.push_str(text);
                        }
                        snapshot.partial.clear();
                    }
                })
            })
            .await
    }

    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            change(&mut inner.snapshot);
            inner.snapshot.clone()
        };
        self.emit(&snapshot);
    }

    fn emit(&self, snapshot: &Snapshot) {
        let _ = self.app.emit("recorder-state", snapshot);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
