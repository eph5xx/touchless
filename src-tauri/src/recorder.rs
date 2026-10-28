use std::{
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};

use crate::{
    db::Db,
    elevenlabs::{self, ElevenLabs, TranscriptEvent},
    external,
    hotkey::{Hotkey, HotkeyStatus},
    microphone::Microphone,
    models::{LocalModel, ModelStore},
    openai::OpenAi,
    overlay::Overlay,
    paste::{self, Delivery},
    plugin_api::{Dictation, Plugin},
    plugin_host::{self, PluginHost},
    settings::{OverlayStyle, Provider, Settings, TranscriptionOptions},
    sound::{Cue, Sounds},
    tray,
    whisper::{self, LocalWhisper},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Keeps the microphone open briefly after stop so the last syllable is captured.
const POST_ROLL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Connecting,
    Listening,
    /// The user stopped; waiting for the final transcript.
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
    transcript_id: Option<i64>,
    delivery: Option<Delivery>,
    error: Option<String>,
    cancelled: bool,
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

    /// Pieces join with a space, or with a blank line when either side is
    /// multi-line, so paragraphs and lists from a rewrite stay apart.
    fn text(&self) -> String {
        let mut joined = String::new();
        let mut previous_multiline = false;
        for text in self
            .0
            .iter()
            .map(|segment| segment.edited.as_deref().unwrap_or(&segment.raw))
            .filter(|text| !text.is_empty())
        {
            let multiline = text.contains('\n');
            if !joined.is_empty() {
                joined.push_str(if multiline || previous_multiline {
                    "\n\n"
                } else {
                    " "
                });
            }
            joined.push_str(text);
            previous_multiline = multiline;
        }
        joined
    }
}

struct Inner {
    snapshot: Snapshot,
    segments: Segments,
    /// Languages ElevenLabs detected for the committed pieces.
    languages: Vec<String>,
    /// From the overlay's `show` for this dictation; `None` when the pop-up is off.
    overlay_token: Option<u64>,
    stop: Option<std_mpsc::Sender<()>>,
    cancel: Option<oneshot::Sender<()>>,
    /// Set once the current dictation is cancelled; also aborts local transcription.
    cancelled: Arc<AtomicBool>,
}

pub struct Recorder {
    app: AppHandle,
    overlay: Overlay,
    sounds: Sounds,
    inner: Mutex<Inner>,
}

impl Recorder {
    pub fn new(app: AppHandle, overlay: Overlay, sounds: Sounds) -> Self {
        Self {
            app,
            overlay,
            sounds,
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
                languages: Vec::new(),
                overlay_token: None,
                stop: None,
                cancel: None,
                cancelled: Arc::new(AtomicBool::new(false)),
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
        if inner.snapshot.status != Status::Idle && inner.cancelled.load(Ordering::SeqCst) {
            return;
        }
        match inner.snapshot.status {
            Status::Idle => {
                let (stop_tx, stop_rx) = std_mpsc::channel();
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let cancelled = Arc::new(AtomicBool::new(false));
                inner.stop = Some(stop_tx);
                inner.cancel = Some(cancel_tx);
                inner.cancelled = Arc::clone(&cancelled);
                inner.segments = Segments::default();
                inner.languages.clear();
                let snapshot = &mut inner.snapshot;
                snapshot.status = Status::Connecting;
                snapshot.started_at = Some(now_ms());
                snapshot.committed.clear();
                snapshot.partial.clear();
                snapshot.error = None;
                let snapshot = snapshot.clone();
                drop(inner);
                self.emit(&snapshot);

                let preferences = self.app.state::<Settings>().preferences();
                if preferences.play_sounds {
                    self.sounds.play(Cue::Start);
                }
                let token = (preferences.overlay != OverlayStyle::Off).then(|| self.overlay.show());
                self.inner.lock().unwrap().overlay_token = token;

                let recorder = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    recorder.session(stop_rx, cancel_rx, cancelled).await
                });
            }
            Status::Connecting | Status::Listening => {
                if let Some(stop) = inner.stop.take() {
                    let _ = stop.send(());
                }
                inner.snapshot.status = Status::Finalizing;
                let snapshot = inner.snapshot.clone();
                drop(inner);
                self.emit(&snapshot);
                if self.app.state::<Settings>().preferences().play_sounds {
                    self.sounds.play(Cue::Stop);
                }
            }
            Status::Finalizing => {
                drop(inner);
                let _ = self.app.emit("toggle-ignored", ());
            }
        }
    }

    /// Stops the current dictation without saving, pasting or running plugins.
    pub fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.snapshot.status == Status::Idle || inner.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(cancel) = inner.cancel.take() {
            let _ = cancel.send(());
        }
        // Dropping the sender closes the microphone.
        inner.stop = None;
    }

    async fn session(
        self: Arc<Self>,
        stop: std_mpsc::Receiver<()>,
        cancel: oneshot::Receiver<()>,
        cancelled: Arc<AtomicBool>,
    ) {
        let result = tokio::select! {
            result = self.record(stop, Arc::clone(&cancelled)) => Some(result),
            _ = cancel => None,
        };
        // A cancel that lands as the recording finishes still wins.
        let result = match result {
            Some(result) if !cancelled.load(Ordering::SeqCst) => result,
            _ => return self.cancelled(),
        };

        let (text, started_at, languages, overlay_token) = {
            let mut inner = self.inner.lock().unwrap();
            // Dropping the sender also stops the microphone thread on error paths.
            inner.stop = None;
            inner.cancel = None;
            (
                inner.snapshot.committed.trim().to_owned(),
                inner.snapshot.started_at.unwrap_or_else(now_ms),
                std::mem::take(&mut inner.languages),
                inner.overlay_token,
            )
        };

        let mut error = result.err().map(|error| error.to_string());
        let mut delivery = None;
        let mut transcript_id = None;
        if !text.is_empty() {
            let db = self.app.state::<Db>();
            match db.insert(&text, started_at, now_ms() - started_at) {
                Ok(transcript) => {
                    transcript_id = Some(transcript.id);
                    let _ = self.app.emit("transcripts-changed", ());
                    tray::transcripts_changed(&self.app);
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
                transcript_id,
                delivery,
                error: error.clone(),
                cancelled: false,
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

        if let Some(token) = overlay_token {
            self.overlay.hide_after(token, hide_delay);
        }

        // Plugins run in the background after the paste; nothing waits for them.
        if let Some((plugins, dictation)) =
            transcript_id.and_then(|id| self.plugins_for(id, &text, &languages))
        {
            let app = self.app.clone();
            tauri::async_runtime::spawn(async move {
                plugin_host::run(&app, plugins, dictation).await;
            });
        }
    }

    fn cancelled(&self) {
        let overlay_token = {
            let mut inner = self.inner.lock().unwrap();
            inner.stop = None;
            inner.cancel = None;
            inner.segments = Segments::default();
            inner.languages.clear();
            inner.overlay_token
        };
        let _ = self.app.emit(
            "dictation-finished",
            Finished {
                text: String::new(),
                transcript_id: None,
                delivery: None,
                error: None,
                cancelled: true,
            },
        );
        self.update(|snapshot| {
            snapshot.status = Status::Idle;
            snapshot.committed.clear();
            snapshot.partial.clear();
            snapshot.error = None;
        });
        if let Some(token) = overlay_token {
            self.overlay.hide_after(token, Duration::from_millis(1_200));
        }
    }

    fn plugins_for(
        &self,
        transcript_id: i64,
        text: &str,
        languages: &[String],
    ) -> Option<(Vec<Arc<dyn Plugin>>, Dictation)> {
        let settings = self.app.state::<Settings>();
        let dictation = Dictation {
            transcript_id,
            text: text.to_owned(),
            language: dictation_language(languages, &settings.transcription()),
        };
        let plugins = self
            .app
            .state::<PluginHost>()
            .applicable(&plugin_host::available(&settings), &dictation);
        (!plugins.is_empty()).then_some((plugins, dictation))
    }

    async fn record(
        self: &Arc<Self>,
        stop: std_mpsc::Receiver<()>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let settings = self.app.state::<Settings>();
        let options = settings.transcription();
        match settings.engine().local() {
            None => self.record_elevenlabs(stop, options).await,
            Some(model) => self.record_whisper(stop, model, options, cancelled).await,
        }
    }

    async fn record_elevenlabs(
        self: &Arc<Self>,
        stop: std_mpsc::Receiver<()>,
        options: TranscriptionOptions,
    ) -> Result<()> {
        let api_key = self
            .app
            .state::<Settings>()
            .api_key(Provider::ElevenLabs)
            .ok_or("Add your ElevenLabs API key in Settings to start dictating.")?;

        let (sample_rate, audio_rx) = open_microphone(stop).await?;
        if !elevenlabs::SAMPLE_RATES.contains(&sample_rate) {
            return Err(format!(
                "the default microphone sample rate ({sample_rate}) is not supported by ElevenLabs"
            )
            .into());
        }
        let client = ElevenLabs::connect(&api_key, sample_rate, &options).await?;

        self.update(|snapshot| {
            if snapshot.status == Status::Connecting {
                snapshot.status = Status::Listening;
            }
        });

        client.run(audio_rx, |event| self.apply(event)).await
    }

    /// Records until stopped, then transcribes the whole recording at once.
    async fn record_whisper(
        self: &Arc<Self>,
        stop: std_mpsc::Receiver<()>,
        model: LocalModel,
        options: TranscriptionOptions,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        let path = match &model {
            LocalModel::Builtin(builtin) => {
                let models = self.app.state::<ModelStore>();
                if !models.is_downloaded(*builtin) {
                    return Err(format!("Download {} in Settings → Models.", builtin.name()).into());
                }
                models.path(*builtin)
            }
            LocalModel::External(path) => {
                if !path.is_file() {
                    return Err(format!(
                        "Touchless can't find {}. Choose another model in Settings → Models.",
                        external::file_name(path)
                    )
                    .into());
                }
                path.clone()
            }
        };

        let (sample_rate, mut audio_rx) = open_microphone(stop).await?;
        // The microphone is already buffering, so nothing is lost while the model loads.
        let whisper = Arc::clone(self.app.state::<Arc<LocalWhisper>>().inner());
        let context = whisper.ready(model.clone(), path).await?;
        self.update(|snapshot| {
            if snapshot.status == Status::Connecting {
                snapshot.status = Status::Listening;
            }
        });

        let mut chunks = Vec::new();
        while let Some(chunk) = audio_rx.recv().await {
            chunks.push(chunk);
        }
        // Whisper tends to invent text such as "Thank you." for silence.
        if !elevenlabs::contains_voice(&chunks, sample_rate) {
            return Ok(());
        }

        let started = std::time::Instant::now();
        let transcription = options.clone();
        let transcript = tauri::async_runtime::spawn_blocking(move || {
            let pcm = whisper::to_whisper_audio(&chunks, sample_rate)?;
            whisper::transcribe(&context, &pcm, &transcription, cancelled)
        })
        .await
        .map_err(|error| format!("Whisper stopped unexpectedly: {error}"))??;
        eprintln!(
            "{}: transcribed in {} ms",
            model.name(),
            started.elapsed().as_millis()
        );

        let text = transcript.text;
        self.apply(TranscriptEvent::Committed(text.clone()));
        if let Some(language) = transcript.language {
            self.apply(TranscriptEvent::Language(language));
        }

        let openai = {
            let settings = self.app.state::<Settings>();
            settings
                .openai_ready()
                .then(|| OpenAi::from_settings(&settings))
        };
        if let (Some(instruction), Some(openai), false) =
            (options.edit_instruction(), openai, text.trim().is_empty())
        {
            match rewrite(openai, instruction, &text).await {
                Ok(edited) => self.apply(TranscriptEvent::Edited {
                    original: text,
                    edited,
                }),
                Err(error) => eprintln!("rewrite failed, keeping the transcript: {error}"),
            }
        }
        Ok(())
    }

    fn apply(&self, event: TranscriptEvent) {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            if let TranscriptEvent::Language(code) = event {
                inner.languages.push(code.to_ascii_lowercase());
                return;
            }
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
                TranscriptEvent::Language(_) => {}
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
        tray::set_status(&self.app, snapshot.status);
        if let Some(hotkey) = self.app.try_state::<Hotkey>() {
            hotkey.set_dictating(snapshot.status != Status::Idle);
        }
    }
}

/// Opens the default microphone; returns its sample rate and the 16-bit mono
/// PCM chunks, which end once `stop` fires (or its sender is dropped).
async fn open_microphone(
    stop: std_mpsc::Receiver<()>,
) -> Result<(u32, mpsc::UnboundedReceiver<Vec<u8>>)> {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
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
    Ok((sample_rate, audio_rx))
}

async fn rewrite(
    openai: std::result::Result<OpenAi, String>,
    instruction: &str,
    text: &str,
) -> std::result::Result<String, String> {
    openai?.rewrite(instruction, text).await
}

/// The language heard most often. Without detection, the main language counts
/// only when it is the only one the user speaks.
fn dictation_language(detected: &[String], options: &TranscriptionOptions) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for code in detected {
        match counts.iter_mut().find(|(seen, _)| *seen == code) {
            Some((_, count)) => *count += 1,
            None => counts.push((code, 1)),
        }
    }
    if let Some((code, _)) = counts.iter().max_by_key(|(_, count)| *count) {
        return Some((*code).to_owned());
    }
    if options.secondary_languages.is_empty() {
        options.language_code.clone()
    } else {
        None
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
    fn keeps_multi_line_pieces_apart() {
        let mut segments = Segments::default();
        segments.commit("intro");
        segments.commit("list");
        segments.commit("outro");
        segments.commit("end");
        segments.edit("list", "- one\n- two");
        assert_eq!(segments.text(), "intro\n\n- one\n- two\n\noutro end");
    }

    #[test]
    fn picks_the_dictation_language() {
        let codes = |codes: &[&str]| {
            codes
                .iter()
                .map(|code| code.to_string())
                .collect::<Vec<_>>()
        };
        let options = |main: Option<&str>, others: &[&str]| TranscriptionOptions {
            language_code: main.map(Into::into),
            secondary_languages: codes(others),
            ..Default::default()
        };

        let detected = codes(&["eng", "rus", "eng"]);
        assert_eq!(
            dictation_language(&detected, &options(Some("rus"), &[])).as_deref(),
            Some("eng")
        );
        assert_eq!(
            dictation_language(&[], &options(Some("eng"), &[])).as_deref(),
            Some("eng")
        );
        assert_eq!(
            dictation_language(&[], &options(Some("eng"), &["rus"])),
            None
        );
        assert_eq!(dictation_language(&[], &options(None, &[])), None);
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
