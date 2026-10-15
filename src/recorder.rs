use std::{
    error::Error,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};

use crate::{
    db::Db,
    elevenlabs::{ElevenLabs, TranscriptEvent},
    hotkey::HotkeyStatus,
    microphone::Microphone,
    overlay::Overlay,
    paste::{self, Delivery},
    settings::Settings,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Keeps the microphone open briefly after stop so the last syllable is captured.
const POST_ROLL: Duration = Duration::from_millis(150);
/// Room for ~25 s of 100 ms chunks, so audio recorded while the connection is
/// still being established is not dropped.
const AUDIO_BUFFER_CHUNKS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Connecting,
    Listening,
    /// The user stopped; waiting for ElevenLabs to commit the final transcript.
    Finalizing,
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

#[derive(Debug, Clone, Serialize)]
struct Finished {
    text: String,
    delivery: Option<Delivery>,
    error: Option<String>,
}

struct Segment {
    raw: String,
    edited: Option<String>,
}

/// Committed transcript pieces; an `edited_transcript` replaces its piece.
#[derive(Default)]
struct Segments(Vec<Segment>);

impl Segments {
    fn commit(&mut self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.0.push(Segment {
                raw: text.to_owned(),
                edited: None,
            });
        }
    }

    fn edit(&mut self, original: &str, edited: &str) {
        let original = original.trim();
        if let Some(segment) = self
            .0
            .iter_mut()
            .find(|segment| segment.edited.is_none() && segment.raw == original)
        {
            segment.edited = Some(edited.trim().to_owned());
        }
    }

    fn text(&self) -> String {
        self.0
            .iter()
            .map(|segment| segment.edited.as_deref().unwrap_or(&segment.raw))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

struct Inner {
    snapshot: Snapshot,
    segments: Segments,
    stop: Option<std_mpsc::Sender<()>>,
}

pub struct Recorder {
    app: AppHandle,
    overlay: Overlay,
    inner: Mutex<Inner>,
}

impl Recorder {
    pub fn new(app: AppHandle, overlay: Overlay) -> Self {
        Self {
            app,
            overlay,
            inner: Mutex::new(Inner {
                snapshot: Snapshot {
                    status: Status::Idle,
                    started_at: None,
                    committed: String::new(),
                    partial: String::new(),
                    error: None,
                    hotkey: HotkeyStatus::Unavailable,
                },
                segments: Segments::default(),
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
                inner.segments = Segments::default();
                let snapshot = &mut inner.snapshot;
                snapshot.status = Status::Connecting;
                snapshot.started_at = Some(now_ms());
                snapshot.committed.clear();
                snapshot.partial.clear();
                snapshot.error = None;
                let snapshot = snapshot.clone();
                drop(inner);
                self.emit(&snapshot);

                if self.app.state::<Settings>().preferences().show_overlay {
                    self.overlay.show();
                }

                let recorder = Arc::clone(self);
                tauri::async_runtime::spawn(async move { recorder.session(stop_rx).await });
            }
            Status::Connecting | Status::Listening => {
                if let Some(stop) = inner.stop.take() {
                    let _ = stop.send(());
                }
                inner.snapshot.status = Status::Finalizing;
                let snapshot = inner.snapshot.clone();
                drop(inner);
                self.emit(&snapshot);
            }
            Status::Finalizing => {
                drop(inner);
                let _ = self.app.emit("toggle-ignored", ());
            }
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
        let mut delivery = None;
        if !text.is_empty() {
            let db = self.app.state::<Db>();
            match db.insert(&text, started_at, now_ms() - started_at) {
                Ok(_) => {
                    let _ = self.app.emit("transcripts-changed", ());
                }
                Err(db_error) => error = Some(format!("could not save transcript: {db_error}")),
            }

            let preferences = self.app.state::<Settings>().preferences();
            let touchless_focused = self
                .app
                .get_webview_window("main")
                .and_then(|window| window.is_focused().ok())
                .unwrap_or(false);
            match paste::deliver(&text, &preferences, touchless_focused) {
                Ok(outcome) => delivery = Some(outcome),
                Err(paste_error) => {
                    error.get_or_insert(format!("could not paste: {paste_error}"));
                }
            }
        }

        let _ = self.app.emit(
            "dictation-finished",
            Finished {
                text: text.clone(),
                delivery,
                error: error.clone(),
            },
        );
        let hide_delay = match (&error, text.is_empty()) {
            (Some(_), _) => Duration::from_millis(3_500),
            (None, true) => Duration::from_millis(1_200),
            (None, false) => Duration::from_millis(1_400),
        };

        self.update(|snapshot| {
            snapshot.status = Status::Idle;
            snapshot.partial.clear();
            snapshot.error = error;
        });
        self.overlay.hide_after(hide_delay);
    }

    async fn record(self: &Arc<Self>, stop: std_mpsc::Receiver<()>) -> Result<()> {
        let settings = self.app.state::<Settings>();
        let api_key = settings
            .api_key()
            .ok_or("Add your ElevenLabs API key in Settings to start dictating.")?;
        let options = settings.transcription();

        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_BUFFER_CHUNKS);
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
                        std::thread::sleep(POST_ROLL);
                    }
                    Err(error) => {
                        let _ = rate_tx.send(Err(error.to_string()));
                    }
                }
            })?;

        let sample_rate = rate_rx
            .await
            .map_err(|_| "microphone thread exited unexpectedly")??;
        let client = ElevenLabs::connect(&api_key, sample_rate, &options).await?;

        self.update(|snapshot| {
            if snapshot.status == Status::Connecting {
                snapshot.status = Status::Listening;
            }
        });

        client.run(audio_rx, |event| self.apply(event)).await
    }

    fn apply(&self, event: TranscriptEvent) {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            let Inner {
                snapshot, segments, ..
            } = &mut *inner;
            match event {
                TranscriptEvent::Partial(text) => snapshot.partial = text,
                TranscriptEvent::Committed(text) => {
                    segments.commit(&text);
                    snapshot.committed = segments.text();
                    snapshot.partial.clear();
                }
                TranscriptEvent::Edited { original, edited } => {
                    segments.edit(&original, &edited);
                    snapshot.committed = segments.text();
                }
            }
            snapshot.clone()
        };
        self.emit(&snapshot);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_committed_segments() {
        let mut segments = Segments::default();
        segments.commit(" hello ");
        segments.commit("");
        segments.commit("world");
        assert_eq!(segments.text(), "hello world");
    }

    #[test]
    fn applies_edits_out_of_order() {
        let mut segments = Segments::default();
        segments.commit("one");
        segments.commit("two");
        segments.commit("one");
        segments.edit("two", "Two.");
        segments.edit("one", "One.");
        assert_eq!(segments.text(), "One. Two. one");
        segments.edit("one", "One!");
        assert_eq!(segments.text(), "One. Two. One!");
    }

    #[test]
    fn ignores_unknown_edits_and_drops_emptied_segments() {
        let mut segments = Segments::default();
        segments.commit("um");
        segments.commit("hi");
        segments.edit("never committed", "x");
        segments.edit("um", "");
        assert_eq!(segments.text(), "hi");
    }
}
