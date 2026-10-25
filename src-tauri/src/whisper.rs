//! Speech recognition on this Mac with whisper.cpp.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::{models::LocalModel, settings::TranscriptionOptions};

pub const SAMPLE_RATE: u32 = 16_000;
/// whisper.cpp's default beam width.
const BEAM_SIZE: i32 = 5;
const NO_SPEECH_THRESHOLD: f32 = 0.6;

/// ISO-639-3 codes paired with Whisper's codes. The first entry for a Whisper
/// code is the one reported back, so `cmn` wins over other Chinese codes.
const LANGUAGES: &[(&str, &str)] = &[
    ("afr", "af"),
    ("amh", "am"),
    ("ara", "ar"),
    ("asm", "as"),
    ("aze", "az"),
    ("bak", "ba"),
    ("bel", "be"),
    ("bul", "bg"),
    ("ben", "bn"),
    ("bod", "bo"),
    ("bre", "br"),
    ("bos", "bs"),
    ("cat", "ca"),
    ("ces", "cs"),
    ("cym", "cy"),
    ("dan", "da"),
    ("deu", "de"),
    ("ell", "el"),
    ("eng", "en"),
    ("spa", "es"),
    ("est", "et"),
    ("eus", "eu"),
    ("fas", "fa"),
    ("fin", "fi"),
    ("fao", "fo"),
    ("fra", "fr"),
    ("glg", "gl"),
    ("guj", "gu"),
    ("hau", "ha"),
    ("haw", "haw"),
    ("heb", "he"),
    ("hin", "hi"),
    ("hrv", "hr"),
    ("hat", "ht"),
    ("hun", "hu"),
    ("hye", "hy"),
    ("ind", "id"),
    ("isl", "is"),
    ("ita", "it"),
    ("jpn", "ja"),
    ("jav", "jw"),
    ("kat", "ka"),
    ("kaz", "kk"),
    ("khm", "km"),
    ("kan", "kn"),
    ("kor", "ko"),
    ("lat", "la"),
    ("ltz", "lb"),
    ("lin", "ln"),
    ("lao", "lo"),
    ("lit", "lt"),
    ("lav", "lv"),
    ("mlg", "mg"),
    ("mri", "mi"),
    ("mkd", "mk"),
    ("mal", "ml"),
    ("mon", "mn"),
    ("mar", "mr"),
    ("msa", "ms"),
    ("mlt", "mt"),
    ("mya", "my"),
    ("nep", "ne"),
    ("nld", "nl"),
    ("nno", "nn"),
    ("nor", "no"),
    ("oci", "oc"),
    ("pan", "pa"),
    ("pol", "pl"),
    ("pus", "ps"),
    ("por", "pt"),
    ("ron", "ro"),
    ("rus", "ru"),
    ("san", "sa"),
    ("snd", "sd"),
    ("sin", "si"),
    ("slk", "sk"),
    ("slv", "sl"),
    ("sna", "sn"),
    ("som", "so"),
    ("sqi", "sq"),
    ("srp", "sr"),
    ("sun", "su"),
    ("swe", "sv"),
    ("swa", "sw"),
    ("tam", "ta"),
    ("tel", "te"),
    ("tgk", "tg"),
    ("tha", "th"),
    ("tuk", "tk"),
    ("fil", "tl"),
    ("tgl", "tl"),
    ("tur", "tr"),
    ("tat", "tt"),
    ("ukr", "uk"),
    ("urd", "ur"),
    ("uzb", "uz"),
    ("vie", "vi"),
    ("yid", "yi"),
    ("yor", "yo"),
    ("yue", "yue"),
    ("cmn", "zh"),
    ("zho", "zh"),
];

pub fn whisper_language(iso3: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|(code, _)| *code == iso3)
        .map(|(_, whisper)| *whisper)
}

pub fn iso3_language(whisper: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|(_, code)| *code == whisper)
        .map(|(iso3, _)| *iso3)
}

/// The language to force: only when the user speaks exactly one that Whisper knows.
fn forced_language(options: &TranscriptionOptions) -> Option<&'static str> {
    match (
        &options.language_code,
        options.secondary_languages.is_empty(),
    ) {
        (Some(code), true) => whisper_language(code),
        _ => None,
    }
}

/// Custom words as Whisper's initial prompt, which nudges it toward their spelling.
fn initial_prompt(options: &TranscriptionOptions) -> Option<String> {
    let prompt = options
        .keyterms
        .iter()
        .map(|term| term.replace('\0', ""))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    (!prompt.is_empty()).then_some(prompt)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WhisperStatus {
    Unloaded,
    Loading { model: LocalModel },
    Ready { model: LocalModel },
    Failed { model: LocalModel, error: String },
}

struct Loaded {
    model: LocalModel,
    context: Arc<WhisperContext>,
}

struct Inner {
    loaded: Option<Loaded>,
    /// Bumped on every load and unload, so a slow load that was replaced is dropped.
    generation: u64,
}

/// Keeps the selected Whisper model in memory between dictations.
pub struct LocalWhisper {
    app: AppHandle,
    inner: Mutex<Inner>,
    status: watch::Sender<WhisperStatus>,
}

#[derive(Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// ISO-639-3 code of the language Whisper heard.
    pub language: Option<String>,
}

impl LocalWhisper {
    pub fn new(app: AppHandle) -> Arc<Self> {
        // Without a logging backend this keeps whisper.cpp from flooding stderr.
        whisper_rs::install_logging_hooks();
        Arc::new(Self {
            app,
            inner: Mutex::new(Inner {
                loaded: None,
                generation: 0,
            }),
            status: watch::Sender::new(WhisperStatus::Unloaded),
        })
    }

    pub fn status(&self) -> WhisperStatus {
        self.status.borrow().clone()
    }

    fn set_status(&self, status: WhisperStatus) {
        let _ = self.app.emit("whisper-status", &status);
        self.status.send_replace(status);
    }

    /// Loads `model` in the background, replacing the loaded one.
    pub fn load(self: &Arc<Self>, model: LocalModel, path: PathBuf) {
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            // Free the old model before reading the new one, so both are never in memory.
            inner.loaded = None;
            inner.generation
        };
        self.set_status(WhisperStatus::Loading {
            model: model.clone(),
        });

        let whisper = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            let result =
                WhisperContext::new_with_params(&path, WhisperContextParameters::default())
                    .map_err(|error| format!("could not load {}: {error}", model.name()));
            let mut inner = whisper.inner.lock().unwrap();
            if inner.generation != generation {
                return;
            }
            let status = match result {
                Ok(context) => {
                    inner.loaded = Some(Loaded {
                        model: model.clone(),
                        context: Arc::new(context),
                    });
                    WhisperStatus::Ready { model }
                }
                Err(error) => {
                    eprintln!("{error}");
                    WhisperStatus::Failed { model, error }
                }
            };
            drop(inner);
            whisper.set_status(status);
        });
    }

    pub fn unload(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            inner.loaded = None;
        }
        self.set_status(WhisperStatus::Unloaded);
    }

    /// Waits until `model` is loaded, starting the load if another model or none is.
    pub async fn ready(
        self: &Arc<Self>,
        model: LocalModel,
        path: PathBuf,
    ) -> Result<Arc<WhisperContext>, String> {
        let mut status = self.status.subscribe();
        let loading = matches!(
            &*status.borrow(),
            WhisperStatus::Loading { model: m } | WhisperStatus::Ready { model: m } if *m == model
        );
        if !loading {
            self.load(model.clone(), path);
        }
        loop {
            match status.borrow_and_update().clone() {
                WhisperStatus::Ready { model: ready } if ready == model => {
                    if let Some(loaded) = self.inner.lock().unwrap().loaded.as_ref()
                        && loaded.model == model
                    {
                        return Ok(Arc::clone(&loaded.context));
                    }
                }
                WhisperStatus::Failed {
                    model: failed,
                    error,
                } if failed == model => {
                    return Err(error);
                }
                WhisperStatus::Loading { model: loading } if loading == model => {}
                _ => return Err(format!("{} was unloaded", model.name())),
            }
            status
                .changed()
                .await
                .map_err(|_| format!("{} was unloaded", model.name()))?;
        }
    }
}

/// Transcribes 16 kHz mono audio in one pass. Blocks for a while; run it off the async runtime.
pub fn transcribe(
    context: &WhisperContext,
    pcm: &[f32],
    options: &TranscriptionOptions,
) -> Result<Transcript, String> {
    let mut state = context
        .create_state()
        .map_err(|error| format!("could not start Whisper: {error}"))?;

    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: BEAM_SIZE,
        patience: -1.0,
    });
    params.set_language(Some(forced_language(options).unwrap_or("auto")));
    params.set_no_context(true);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_no_speech_thold(NO_SPEECH_THRESHOLD);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    let prompt = initial_prompt(options);
    if let Some(prompt) = &prompt {
        params.set_initial_prompt(prompt);
    }

    state
        .full(params, pcm)
        .map_err(|error| format!("Whisper could not transcribe: {error}"))?;

    let text = state
        .as_iter()
        .filter_map(|segment| segment.to_str_lossy().ok().map(|text| text.into_owned()))
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let language = whisper_rs::get_lang_str(state.full_lang_id_from_state())
        .and_then(iso3_language)
        .map(str::to_owned);
    Ok(Transcript { text, language })
}

/// Converts 16-bit mono PCM chunks at `sample_rate` to 16 kHz floats.
pub fn to_whisper_audio(chunks: &[Vec<u8>], sample_rate: u32) -> Result<Vec<f32>, String> {
    let samples: Vec<f32> = chunks
        .iter()
        .flat_map(|chunk| chunk.as_chunks::<2>().0.iter())
        .map(|bytes| f32::from(i16::from_le_bytes(*bytes)) / 32_768.0)
        .collect();
    resample(&samples, sample_rate)
}

fn resample(samples: &[f32], sample_rate: u32) -> Result<Vec<f32>, String> {
    if sample_rate == SAMPLE_RATE || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let error = |error: &dyn std::fmt::Display| format!("could not resample audio: {error}");
    let mut resampler = Fft::<f32>::new(
        sample_rate as usize,
        SAMPLE_RATE as usize,
        1024,
        1,
        FixedSync::Both,
    )
    .map_err(|e| error(&e))?;
    let input = InterleavedSlice::new(samples, 1, samples.len()).map_err(|e| error(&e))?;
    let output = resampler
        .process_all(&input, samples.len(), None)
        .map_err(|e| error(&e))?;
    Ok(output.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_languages_both_ways() {
        assert_eq!(whisper_language("eng"), Some("en"));
        assert_eq!(whisper_language("cmn"), Some("zh"));
        assert_eq!(whisper_language("jav"), Some("jw"));
        assert_eq!(whisper_language("yue"), Some("yue"));
        assert_eq!(whisper_language("zul"), None);
        assert_eq!(iso3_language("en"), Some("eng"));
        assert_eq!(iso3_language("zh"), Some("cmn"));
        assert_eq!(iso3_language("tl"), Some("fil"));
        assert_eq!(iso3_language("xx"), None);
        for (iso3, whisper) in LANGUAGES {
            assert_eq!(whisper_language(iso3), Some(*whisper));
            assert!(whisper_rs::get_lang_id(whisper).is_some(), "{whisper}");
        }
    }

    #[test]
    fn forces_a_language_only_when_there_is_one() {
        let options = |main: Option<&str>, others: &[&str]| TranscriptionOptions {
            language_code: main.map(Into::into),
            secondary_languages: others.iter().map(|code| code.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(forced_language(&options(Some("rus"), &[])), Some("ru"));
        assert_eq!(forced_language(&options(Some("rus"), &["eng"])), None);
        assert_eq!(forced_language(&options(None, &[])), None);
        assert_eq!(forced_language(&options(Some("zul"), &[])), None);
    }

    #[test]
    fn custom_words_become_the_prompt() {
        let options = TranscriptionOptions {
            keyterms: vec!["Touchless".into(), "Scribe\0".into(), "\0".into()],
            ..Default::default()
        };
        assert_eq!(
            initial_prompt(&options).as_deref(),
            Some("Touchless, Scribe")
        );
        assert_eq!(initial_prompt(&TranscriptionOptions::default()), None);
    }

    #[test]
    fn resamples_to_16_khz() {
        let one_second: Vec<u8> = (0..48_000)
            .flat_map(|index| ((index % 100) as i16 * 100).to_le_bytes())
            .collect();
        let chunks: Vec<Vec<u8>> = one_second.chunks(9_600).map(<[u8]>::to_vec).collect();
        let audio = to_whisper_audio(&chunks, 48_000).unwrap();
        assert_eq!(audio.len(), 16_000);
        let audio = to_whisper_audio(&chunks, 16_000).unwrap();
        assert_eq!(audio.len(), 48_000);
        assert!(audio.iter().all(|sample| (-1.0..1.0).contains(sample)));
    }

    /// 16 kHz mono PCM of `text` spoken by macOS `say`.
    fn say(text: &str) -> Vec<u8> {
        let path =
            std::env::temp_dir().join(format!("touchless-whisper-{}.wav", std::process::id()));
        let status = std::process::Command::new("say")
            .args(["--file-format=WAVE", "--data-format=LEI16@16000", "-o"])
            .arg(&path)
            .arg(text)
            .status()
            .expect("macOS `say` is needed for live tests");
        assert!(status.success());
        let wav = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let start = wav
            .windows(4)
            .position(|window| window == b"data")
            .expect("WAV data chunk")
            + 8;
        wav[start..].to_vec()
    }

    #[test]
    #[ignore = "needs ggml-small.bin in the Touchless models folder or at $WHISPER_SMALL"]
    fn live_transcribes_with_whisper_small() {
        let path = std::env::var_os("WHISPER_SMALL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap())
                    .join("Library/Application Support/com.touchless.desktop/models")
                    .join(crate::models::WhisperModel::WhisperSmall.spec().file)
            });
        assert!(
            path.is_file(),
            "download Whisper Small first: {}",
            path.display()
        );
        let context =
            WhisperContext::new_with_params(&path, WhisperContextParameters::default()).unwrap();
        let pcm = to_whisper_audio(
            &[say("The quick brown fox jumps over the lazy dog.")],
            16_000,
        )
        .unwrap();
        let transcript = transcribe(&context, &pcm, &TranscriptionOptions::default()).unwrap();
        eprintln!("live (whisper small): {transcript:?}");
        let text = transcript.text.to_lowercase();
        assert!(text.contains("fox"), "{text}");
        assert!(text.contains("lazy dog"), "{text}");
        assert_eq!(transcript.language.as_deref(), Some("eng"));
    }
}
