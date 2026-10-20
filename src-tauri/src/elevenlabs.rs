use std::{collections::VecDeque, error::Error, io, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use tokio::{sync::mpsc, time::Instant};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use url::Url;

use crate::settings::TranscriptionOptions;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const REALTIME_URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
pub const SAMPLE_RATES: [u32; 6] = [16_000, 48_000, 44_100, 24_000, 22_050, 8_000];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Safety limit for waiting on unanswered commits after the user stops.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for waiting on outstanding `edited_transcript` events after the
/// user stops, when an edit instruction is set.
const EDIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Quiet after speech that ends a segment; matches ElevenLabs's VAD default.
const PAUSE: Duration = Duration::from_millis(1_500);
/// Long segments are committed at the next quiet chunk after this much audio.
const QUIET_COMMIT_AFTER: Duration = Duration::from_secs(20);
/// Commit regardless of the level, ahead of the server's own commit at ~36 s,
/// which would not answer one of ours.
const FORCE_COMMIT_AFTER: Duration = Duration::from_secs(30);
/// The background level is the quietest chunk within this much recent audio.
const BACKGROUND_WINDOW: Duration = Duration::from_secs(5);
/// A chunk is speech when its RMS is this many times the background level...
const SPEECH_RATIO: f64 = 3.0;
/// ...and at least this loud (in 16-bit sample units, about -50 dBFS).
const MIN_SPEECH_LEVEL: f64 = 100.0;
/// Lower bar for "something may have been said", used to decide whether the
/// final commit is needed, so soft last words are never skipped.
const VOICE_RATIO: f64 = 2.0;

#[derive(Debug, PartialEq)]
pub enum TranscriptEvent {
    Partial(String),
    Committed(String),
    /// `original` is the committed text the edit instruction was applied to.
    Edited {
        original: String,
        edited: String,
    },
    /// Language detected for a committed piece; arrives shortly after the commit.
    Language(String),
}

#[derive(Debug, PartialEq)]
enum ServerMessage {
    SessionStarted(serde_json::Value),
    Partial(String),
    Committed(String),
    Edited { text: String, edited: String },
    Language(String),
    Error(String),
    Other,
}

fn parse_server_message(text: &str) -> ServerMessage {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
        return ServerMessage::Other;
    };
    let message_type = value["message_type"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();

    match message_type.as_str() {
        "session_started" => ServerMessage::SessionStarted(value["config"].take()),
        "partial_transcript" => ServerMessage::Partial(field("text")),
        "committed_transcript" => ServerMessage::Committed(field("text")),
        "edited_transcript" => ServerMessage::Edited {
            text: field("text"),
            edited: field("edited_text"),
        },
        "committed_transcript_with_timestamps" => ServerMessage::Language(field("language_code")),
        _ => match value["error"].as_str() {
            Some(error) => ServerMessage::Error(format!("{message_type}: {error}")),
            None => ServerMessage::Other,
        },
    }
}

fn realtime_url(base_url: &str, sample_rate: u32, options: &TranscriptionOptions) -> Result<Url> {
    let mut url = Url::parse(base_url)?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("model_id", "scribe_v2_realtime")
            .append_pair("audio_format", &format!("pcm_{sample_rate}"))
            .append_pair("commit_strategy", "manual")
            .append_pair("include_language_detection", "true");
        if let Some(code) = &options.language_code {
            query.append_pair("language_code", code);
        }
        for code in &options.secondary_languages {
            query.append_pair("secondary_languages", code);
        }
        if options.no_verbatim {
            query.append_pair("no_verbatim", "true");
        }
        for term in &options.keyterms {
            query.append_pair("keyterms", term);
        }
        if let Some(instruction) = options.edit_instruction() {
            query.append_pair("transcript_edit", instruction);
        }
    }
    Ok(url)
}

/// Decides when to commit from the loudness of each 16-bit mono PCM chunk.
struct Segmenter {
    sample_rate: u32,
    /// `(rms, samples)` of recent chunks, covering at least `BACKGROUND_WINDOW`.
    recent: VecDeque<(f64, usize)>,
    recent_samples: usize,
    uncommitted: usize,
    /// Samples of quiet since the last speech chunk.
    quiet: usize,
    speech: bool,
    voice: bool,
    /// Whether any chunk so far may have contained speech; never reset.
    heard_voice: bool,
}

impl Segmenter {
    fn new(sample_rate: u32) -> Self {
        let window = samples(sample_rate, BACKGROUND_WINDOW);
        Self {
            sample_rate,
            // A silent prior holds the background at zero until a full window
            // of real audio has been heard, so speech right at the start counts.
            recent: VecDeque::from([(0.0, window)]),
            recent_samples: window,
            uncommitted: 0,
            quiet: 0,
            speech: false,
            voice: false,
            heard_voice: false,
        }
    }

    /// Returns whether this chunk should be sent with a commit.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let count = chunk.len() / 2;
        if count == 0 {
            return false;
        }
        let level = rms(chunk);
        let background = self
            .recent
            .iter()
            .map(|(level, _)| *level)
            .fold(f64::INFINITY, f64::min);
        let is_speech = level > (background * SPEECH_RATIO).max(MIN_SPEECH_LEVEL);
        if level > (background * VOICE_RATIO).max(MIN_SPEECH_LEVEL) {
            self.voice = true;
            self.heard_voice = true;
        }

        self.recent.push_back((level, count));
        self.recent_samples += count;
        let window = samples(self.sample_rate, BACKGROUND_WINDOW);
        while let Some(&(_, oldest)) = self.recent.front() {
            if self.recent_samples - oldest < window {
                break;
            }
            self.recent.pop_front();
            self.recent_samples -= oldest;
        }

        self.uncommitted += count;
        if is_speech {
            self.speech = true;
            self.quiet = 0;
        } else {
            self.quiet += count;
        }

        let commit = (self.speech && self.quiet >= samples(self.sample_rate, PAUSE))
            || (!is_speech && self.uncommitted >= samples(self.sample_rate, QUIET_COMMIT_AFTER))
            || self.uncommitted >= samples(self.sample_rate, FORCE_COMMIT_AFTER);
        if commit {
            self.committed();
        }
        commit
    }

    /// Whether the audio since the last commit may contain speech.
    fn has_voice(&self) -> bool {
        self.voice
    }

    fn committed(&mut self) {
        self.uncommitted = 0;
        self.quiet = 0;
        self.speech = false;
        self.voice = false;
    }
}

/// Whether 16-bit mono PCM chunks may contain speech, by the same loudness
/// rules that decide ElevenLabs commits.
pub fn contains_voice(chunks: &[Vec<u8>], sample_rate: u32) -> bool {
    let mut segmenter = Segmenter::new(sample_rate);
    for chunk in chunks {
        segmenter.push(chunk);
    }
    segmenter.heard_voice
}

fn samples(sample_rate: u32, duration: Duration) -> usize {
    (sample_rate as u128 * duration.as_millis() / 1_000) as usize
}

fn rms(chunk: &[u8]) -> f64 {
    let (sum, count) = chunk
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| f64::from(i16::from_le_bytes(*bytes)))
        .fold((0.0, 0usize), |(sum, count), sample| {
            (sum + sample * sample, count + 1)
        });
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

pub struct ElevenLabs {
    socket: Socket,
    sample_rate: u32,
    finalize_timeout: Duration,
    edit_timeout: Duration,
    editing: bool,
    /// Committed texts whose `edited_transcript` has not arrived yet.
    awaiting_edits: Vec<String>,
    segmenter: Segmenter,
    /// With manual commits every `committed_transcript` answers one of ours.
    commits_sent: usize,
    commits_answered: usize,
}

impl ElevenLabs {
    pub async fn connect(
        api_key: &str,
        sample_rate: u32,
        options: &TranscriptionOptions,
    ) -> Result<Self> {
        Self::connect_to(REALTIME_URL, api_key, sample_rate, options).await
    }

    async fn connect_to(
        base_url: &str,
        api_key: &str,
        sample_rate: u32,
        options: &TranscriptionOptions,
    ) -> Result<Self> {
        let url = realtime_url(base_url, sample_rate, options)?;
        let mut request = url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert("xi-api-key", HeaderValue::from_str(api_key)?);

        let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| "timed out connecting to ElevenLabs")??;
        Ok(Self {
            socket,
            sample_rate,
            finalize_timeout: FINALIZE_TIMEOUT,
            edit_timeout: EDIT_TIMEOUT,
            editing: options.edit_instruction().is_some(),
            awaiting_edits: Vec::new(),
            segmenter: Segmenter::new(sample_rate),
            commits_sent: 0,
            commits_answered: 0,
        })
    }

    /// Opens a session, waits for the server to accept it and returns the session
    /// config the server reported.
    pub async fn verify(
        api_key: &str,
        options: &TranscriptionOptions,
    ) -> Result<serde_json::Value> {
        let mut client = Self::connect(api_key, 16_000, options).await?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let result = loop {
            let Ok(message) = tokio::time::timeout_at(deadline, client.socket.next()).await else {
                break Err("ElevenLabs did not respond".into());
            };
            let Some(text) = client.handle(message).await? else {
                break Err("ElevenLabs closed the connection".into());
            };
            match parse_server_message(&text) {
                ServerMessage::SessionStarted(config) => break Ok(config),
                ServerMessage::Error(error) => break Err(error.into()),
                _ => {}
            }
        };
        let _ = client.socket.close(None).await;
        result
    }

    /// Streams audio until the channel closes, then commits whatever speech is
    /// still uncommitted and waits until every commit is answered. The latest
    /// partial transcript is reported as committed if the server never confirms
    /// it, so no speech is lost.
    pub async fn run(
        mut self,
        audio: mpsc::UnboundedReceiver<Vec<u8>>,
        mut on_event: impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let result = self.transcribe(audio, &mut on_event).await;
        let _ = self.socket.close(None).await;
        result
    }

    async fn transcribe(
        &mut self,
        audio: mpsc::UnboundedReceiver<Vec<u8>>,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let mut pending = String::new();
        let result = self.stream(audio, &mut pending, on_event).await;
        if !pending.is_empty() {
            on_event(TranscriptEvent::Committed(std::mem::take(&mut pending)));
        }
        result
    }

    async fn stream(
        &mut self,
        mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
        pending: &mut String,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let mut sent_audio = false;
        loop {
            tokio::select! {
                chunk = audio.recv() => {
                    let Some(chunk) = chunk else { break };
                    if chunk.is_empty() { continue }
                    let commit = self.segmenter.push(&chunk);
                    self.send_chunk(&chunk, commit).await?;
                    sent_audio = true;
                }
                message = self.socket.next() => {
                    let Some(text) = self.handle(message).await? else { return Ok(()) };
                    self.dispatch(&text, pending, on_event)?;
                }
            }
        }

        if sent_audio {
            let stopped = Instant::now();
            // Finalization is best effort: the pending partial transcript is still
            // a good result if the server rejects or never answers a commit.
            let hit_limit = self
                .finalize(pending, on_event)
                .await
                .unwrap_or_else(|error| {
                    eprintln!("ElevenLabs finalization: {error}");
                    false
                });
            eprintln!(
                "ElevenLabs: {} of {} commit(s) answered, finished {} ms after stop{}",
                self.commits_answered,
                self.commits_sent,
                stopped.elapsed().as_millis(),
                if hit_limit {
                    " (hit the safety limit)"
                } else {
                    ""
                }
            );
        }
        Ok(())
    }

    /// Returns whether the safety limit was hit with commits still unanswered.
    async fn finalize(
        &mut self,
        pending: &mut String,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<bool> {
        // A commit without new speech gets nothing back but an empty transcript
        // or `commit_throttled`, so only send one when there may be words.
        if self.segmenter.has_voice() {
            self.segmenter.committed();
            self.send_chunk(&[], true).await?;
        }

        let started = Instant::now();
        let commit_deadline = started + self.finalize_timeout;
        let edit_deadline = started + self.edit_timeout;
        loop {
            let answered = self.commits_answered >= self.commits_sent;
            let deadline = match (answered, self.awaiting_edits.is_empty()) {
                (true, true) => return Ok(false),
                (true, false) => edit_deadline,
                (false, true) => commit_deadline,
                (false, false) => commit_deadline.max(edit_deadline),
            };
            let Ok(message) = tokio::time::timeout_at(deadline, self.socket.next()).await else {
                return Ok(!answered);
            };
            let Some(text) = self.handle(message).await? else {
                return Ok(false);
            };
            self.dispatch(&text, pending, on_event)?;
        }
    }

    async fn send_chunk(&mut self, chunk: &[u8], commit: bool) -> Result<()> {
        let message = serde_json::json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": STANDARD.encode(chunk),
            "commit": commit,
            "sample_rate": self.sample_rate,
        });
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await?;
        if commit {
            self.commits_sent += 1;
        }
        Ok(())
    }

    /// Returns `Some(text)` for text frames, `None` when the socket closed.
    async fn handle(
        &mut self,
        message: Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<Option<String>> {
        match message.transpose()? {
            Some(Message::Text(text)) => Ok(Some(text.to_string())),
            Some(Message::Ping(data)) => {
                self.socket.send(Message::Pong(data)).await?;
                Ok(Some(String::new()))
            }
            Some(Message::Close(_)) | None => Ok(None),
            _ => Ok(Some(String::new())),
        }
    }

    fn dispatch(
        &mut self,
        text: &str,
        pending: &mut String,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        match parse_server_message(text) {
            ServerMessage::Partial(text) => {
                pending.clone_from(&text);
                on_event(TranscriptEvent::Partial(text));
            }
            ServerMessage::Committed(text) => {
                self.commits_answered += 1;
                pending.clear();
                if self.editing && !text.trim().is_empty() {
                    self.awaiting_edits.push(text.clone());
                }
                on_event(TranscriptEvent::Committed(text));
            }
            ServerMessage::Edited { text, edited } => {
                // Edits may arrive out of commit order, so match them by text.
                if let Some(index) = self.awaiting_edits.iter().position(|item| *item == text) {
                    self.awaiting_edits.remove(index);
                }
                on_event(TranscriptEvent::Edited {
                    original: text,
                    edited,
                });
            }
            ServerMessage::Language(code) if !code.trim().is_empty() => {
                on_event(TranscriptEvent::Language(code.trim().to_owned()));
            }
            ServerMessage::Error(error) => return Err(io::Error::other(error).into()),
            ServerMessage::SessionStarted(_)
            | ServerMessage::Language(_)
            | ServerMessage::Other => {}
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Rewrite;
    use tokio::net::TcpListener;

    #[test]
    fn parses_transcripts() {
        assert_eq!(
            parse_server_message(r#"{"message_type":"partial_transcript","text":"hel"}"#),
            ServerMessage::Partial("hel".into())
        );
        assert_eq!(
            parse_server_message(r#"{"message_type":"committed_transcript","text":"hello"}"#),
            ServerMessage::Committed("hello".into())
        );
    }

    #[test]
    fn parses_errors() {
        assert_eq!(
            parse_server_message(r#"{"message_type":"auth_error","error":"bad key"}"#),
            ServerMessage::Error("auth_error: bad key".into())
        );
    }

    #[test]
    fn parses_session_start_and_ignores_others() {
        assert_eq!(
            parse_server_message(
                r#"{"message_type":"session_started","session_id":"x","config":{"language_code":"eng"}}"#
            ),
            ServerMessage::SessionStarted(serde_json::json!({"language_code": "eng"}))
        );
        assert_eq!(
            parse_server_message(r#"{"message_type":"warning","warning":"logged"}"#),
            ServerMessage::Other
        );
        assert_eq!(parse_server_message("not json"), ServerMessage::Other);
    }

    #[test]
    fn parses_edited_transcripts() {
        assert_eq!(
            parse_server_message(
                r#"{"message_type":"edited_transcript","text":"hi there","edited_text":"Hi there."}"#
            ),
            ServerMessage::Edited {
                text: "hi there".into(),
                edited: "Hi there.".into()
            }
        );
    }

    #[test]
    fn parses_detected_language() {
        assert_eq!(
            parse_server_message(
                r#"{"message_type":"committed_transcript_with_timestamps","text":"hi","language_code":"eng","words":[]}"#
            ),
            ServerMessage::Language("eng".into())
        );
    }

    #[test]
    fn builds_url_with_default_options() {
        let url = realtime_url(REALTIME_URL, 48_000, &TranscriptionOptions::default()).unwrap();
        assert_eq!(
            url.as_str(),
            "wss://api.elevenlabs.io/v1/speech-to-text/realtime?model_id=scribe_v2_realtime&audio_format=pcm_48000&commit_strategy=manual&include_language_detection=true"
        );
    }

    #[test]
    fn builds_url_with_all_options() {
        let options = TranscriptionOptions {
            language_code: Some("eng".into()),
            secondary_languages: vec!["rus".into(), "ukr".into()],
            no_verbatim: true,
            keyterms: vec!["Touchless".into(), "Scribe v2 & co".into()],
            rewrite: Rewrite::Custom,
            transcript_edit: "Fix grammar.".into(),
        };
        let url = realtime_url(REALTIME_URL, 16_000, &options).unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let expected = [
            ("model_id", "scribe_v2_realtime"),
            ("audio_format", "pcm_16000"),
            ("commit_strategy", "manual"),
            ("include_language_detection", "true"),
            ("language_code", "eng"),
            ("secondary_languages", "rus"),
            ("secondary_languages", "ukr"),
            ("no_verbatim", "true"),
            ("keyterms", "Touchless"),
            ("keyterms", "Scribe v2 & co"),
            ("transcript_edit", "Fix grammar."),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        assert_eq!(pairs, expected);
    }

    #[test]
    fn sends_the_selected_rewrite() {
        let edit_param = |rewrite| {
            let options = TranscriptionOptions {
                rewrite,
                transcript_edit: "Fix grammar.".into(),
                ..Default::default()
            };
            let url = realtime_url(REALTIME_URL, 16_000, &options).unwrap();
            url.query_pairs()
                .find(|(key, _)| key == "transcript_edit")
                .map(|(_, value)| value.into_owned())
        };
        assert_eq!(edit_param(Rewrite::Off), None);
        let enhance = TranscriptionOptions {
            rewrite: Rewrite::Enhance,
            ..Default::default()
        };
        assert_eq!(
            edit_param(Rewrite::Enhance).as_deref(),
            enhance.edit_instruction()
        );
        assert_eq!(edit_param(Rewrite::Custom).as_deref(), Some("Fix grammar."));
    }

    const RATE: u32 = 16_000;
    /// 100 ms of samples at `RATE`.
    const CHUNK_SAMPLES: usize = 1_600;

    /// A 100 ms chunk with the given RMS (a square wave of that amplitude).
    fn tone(level: i16) -> Vec<u8> {
        (0..CHUNK_SAMPLES)
            .flat_map(|index| {
                let sample = if index % 2 == 0 { level } else { -level };
                sample.to_le_bytes()
            })
            .collect()
    }

    fn speech(chunks: usize) -> Vec<Vec<u8>> {
        vec![tone(3_000); chunks]
    }

    fn silence(chunks: usize) -> Vec<Vec<u8>> {
        vec![tone(0); chunks]
    }

    /// Indexes of the chunks the segmenter commits with.
    fn commits(segmenter: &mut Segmenter, chunks: &[Vec<u8>]) -> Vec<usize> {
        chunks
            .iter()
            .enumerate()
            .filter_map(|(index, chunk)| segmenter.push(chunk).then_some(index))
            .collect()
    }

    #[test]
    fn commits_after_a_pause_that_follows_speech() {
        let audio = [speech(20), silence(30)].concat();
        let mut segmenter = Segmenter::new(RATE);
        assert_eq!(commits(&mut segmenter, &audio), [20 + 14]);
        assert!(!segmenter.has_voice());
    }

    #[test]
    fn detects_voice_in_a_whole_recording() {
        assert!(!contains_voice(&silence(30), RATE));
        assert!(!contains_voice(&[], RATE));
        assert!(contains_voice(
            &[silence(10), speech(2), silence(30)].concat(),
            RATE
        ));
    }

    #[test]
    fn does_not_commit_silence_alone() {
        let mut segmenter = Segmenter::new(RATE);
        assert_eq!(commits(&mut segmenter, &silence(150)), Vec::<usize>::new());
        assert!(!segmenter.has_voice());
    }

    #[test]
    fn forces_commits_on_long_segments() {
        // Quiet audio is committed once 20 s are uncommitted.
        let mut segmenter = Segmenter::new(RATE);
        assert_eq!(commits(&mut segmenter, &silence(250)), [199]);

        // Speech that never pauses is cut at 30 s: louder than the quietest chunk
        // of the last 5 s by more than the speech ratio throughout.
        let mut segmenter = Segmenter::new(RATE);
        let rising: Vec<Vec<u8>> = (0..150)
            .map(|index| tone((200.0 * 4f64.powf(index as f64 / 50.0)) as i16))
            .collect();
        let audio = [silence(150), rising].concat();
        assert_eq!(commits(&mut segmenter, &audio), [299]);
    }

    #[test]
    fn adapts_to_background_noise() {
        let mut seed = 1u32;
        let mut noise = |amplitude: f64| -> Vec<u8> {
            (0..CHUNK_SAMPLES)
                .flat_map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let unit = f64::from(seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
                    ((unit * amplitude) as i16).to_le_bytes()
                })
                .collect()
        };
        let mut segmenter = Segmenter::new(RATE);
        let background: Vec<Vec<u8>> = (0..150).map(|_| noise(600.0)).collect();
        // The noise counts as speech until it has been heard for a whole window,
        // then ends that segment once and is quiet afterwards.
        let early = commits(&mut segmenter, &background);
        assert_eq!(early.len(), 1);
        assert!((50..=70).contains(&early[0]), "{early:?}");
        assert!(!segmenter.has_voice());

        let mut audio = speech(10);
        audio.extend((0..20).map(|_| noise(600.0)));
        assert_eq!(commits(&mut segmenter, &audio), [10 + 14]);
    }

    /// Fake realtime server: for every received audio message, `respond` returns the
    /// JSON replies to send (given whether that message was a commit).
    async fn fake_server(
        mut respond: impl FnMut(bool) -> Vec<(u64, &'static str)> + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                for (delay_ms, reply) in respond(value["commit"].as_bool().unwrap_or(false)) {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    if socket.send(Message::Text(reply.into())).await.is_err() {
                        return;
                    }
                }
            }
        });
        format!("ws://{address}/realtime")
    }

    /// Answers the n-th commit (from 1) with `replies(n)`.
    fn per_commit(
        replies: impl Fn(usize) -> Vec<(u64, &'static str)> + Send + 'static,
    ) -> impl FnMut(bool) -> Vec<(u64, &'static str)> + Send + 'static {
        let mut commits = 0;
        move |commit| {
            if commit {
                commits += 1;
                replies(commits)
            } else {
                vec![]
            }
        }
    }

    fn committed(text: &str) -> &'static str {
        Box::leak(
            serde_json::json!({"message_type": "committed_transcript", "text": text})
                .to_string()
                .into_boxed_str(),
        )
    }

    fn edited(text: &str, edited: &str) -> &'static str {
        Box::leak(
            serde_json::json!({
                "message_type": "edited_transcript",
                "text": text,
                "edited_text": edited,
            })
            .to_string()
            .into_boxed_str(),
        )
    }

    async fn transcribe(url: &str, audio: Vec<Vec<u8>>, finalize_timeout: Duration) -> Vec<String> {
        let events = transcribe_events(
            url,
            audio,
            &TranscriptionOptions::default(),
            finalize_timeout,
            EDIT_TIMEOUT,
        )
        .await;
        events
            .into_iter()
            .filter_map(|event| match event {
                TranscriptEvent::Committed(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    async fn transcribe_events(
        url: &str,
        audio: Vec<Vec<u8>>,
        options: &TranscriptionOptions,
        finalize_timeout: Duration,
        edit_timeout: Duration,
    ) -> Vec<TranscriptEvent> {
        let mut client = ElevenLabs::connect_to(url, "key", RATE, options)
            .await
            .unwrap();
        client.finalize_timeout = finalize_timeout;
        client.edit_timeout = edit_timeout;
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        for chunk in audio {
            audio_tx.send(chunk).unwrap();
        }
        drop(audio_tx);

        let mut events = Vec::new();
        client
            .run(audio_rx, |event| {
                if !matches!(event, TranscriptEvent::Partial(_)) {
                    events.push(event);
                }
            })
            .await
            .unwrap();
        events
    }

    fn editing() -> TranscriptionOptions {
        TranscriptionOptions {
            rewrite: Rewrite::Custom,
            transcript_edit: "Fix punctuation.".into(),
            ..Default::default()
        }
    }

    /// Speech, a pause long enough for a commit, then the last words.
    fn two_segments() -> Vec<Vec<u8>> {
        [speech(5), silence(15), speech(5)].concat()
    }

    #[tokio::test]
    async fn keeps_a_final_answer_that_arrives_late() {
        let url = fake_server(per_commit(|commit| match commit {
            1 => vec![(10, committed("first"))],
            _ => vec![(1_000, committed("last words"))],
        }))
        .await;

        let committed = transcribe(&url, two_segments(), FINALIZE_TIMEOUT).await;
        assert_eq!(committed, ["first", "last words"]);
    }

    #[tokio::test]
    async fn finishes_right_after_the_last_answer() {
        let url = fake_server(per_commit(|_| vec![(10, committed("done"))])).await;

        let started = Instant::now();
        let committed = transcribe(&url, speech(3), FINALIZE_TIMEOUT).await;
        assert_eq!(committed, ["done"]);
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[tokio::test]
    async fn counts_empty_answers() {
        let url = fake_server(per_commit(|commit| match commit {
            1 => vec![(0, committed(""))],
            _ => vec![(20, committed("done"))],
        }))
        .await;

        let started = Instant::now();
        let committed = transcribe(&url, two_segments(), Duration::from_secs(5)).await;
        assert_eq!(committed, ["", "done"]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn skips_the_final_commit_after_a_pause() {
        let url = fake_server(per_commit(|_| vec![(10, committed("only"))])).await;

        let started = Instant::now();
        let audio = [speech(5), silence(20)].concat();
        let committed = transcribe(&url, audio, Duration::from_secs(5)).await;
        assert_eq!(committed, ["only"]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn waits_for_edits_after_the_last_answer() {
        let url = fake_server(per_commit(|_| {
            vec![
                (10, committed("hi there")),
                (600, edited("hi there", "Hi there.")),
            ]
        }))
        .await;

        let events =
            transcribe_events(&url, speech(1), &editing(), FINALIZE_TIMEOUT, EDIT_TIMEOUT).await;
        assert_eq!(
            events,
            [
                TranscriptEvent::Committed("hi there".into()),
                TranscriptEvent::Edited {
                    original: "hi there".into(),
                    edited: "Hi there.".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn stops_waiting_once_all_edits_arrived() {
        let url = fake_server(per_commit(|commit| match commit {
            1 => vec![(10, committed("one"))],
            _ => vec![
                (10, committed("two")),
                (10, edited("two", "Two.")),
                (10, edited("one", "One.")),
            ],
        }))
        .await;

        let started = Instant::now();
        let events = transcribe_events(
            &url,
            two_segments(),
            &editing(),
            FINALIZE_TIMEOUT,
            Duration::from_secs(30),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(events.len(), 4);
    }

    #[tokio::test]
    async fn keeps_raw_text_when_edit_never_arrives() {
        let url = fake_server(per_commit(|_| vec![(10, committed("raw text"))])).await;

        let events = transcribe_events(
            &url,
            speech(1),
            &editing(),
            FINALIZE_TIMEOUT,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(events, [TranscriptEvent::Committed("raw text".into())]);
    }

    #[tokio::test]
    async fn commits_short_clip_before_any_partial_arrived() {
        let url = fake_server(per_commit(|_| vec![(50, committed("hi there"))])).await;

        let committed = transcribe(&url, speech(2), FINALIZE_TIMEOUT).await;
        assert_eq!(committed, ["hi there"]);
    }

    #[tokio::test]
    async fn falls_back_to_partial_at_the_safety_limit() {
        let url = fake_server(|commit| {
            if commit {
                vec![]
            } else {
                vec![(
                    0,
                    r#"{"message_type":"partial_transcript","text":"almost"}"#,
                )]
            }
        })
        .await;

        let started = Instant::now();
        let committed = transcribe(&url, speech(1), Duration::from_millis(300)).await;
        assert_eq!(committed, ["almost"]);
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn falls_back_to_partial_when_commit_is_rejected() {
        let url = fake_server(|commit| {
            if commit {
                vec![(
                    0,
                    r#"{"message_type":"commit_throttled","error":"too short"}"#,
                )]
            } else {
                vec![(
                    0,
                    r#"{"message_type":"partial_transcript","text":"almost"}"#,
                )]
            }
        })
        .await;

        let committed = transcribe(&url, speech(1), Duration::from_millis(300)).await;
        assert_eq!(committed, ["almost"]);
    }

    /// 16 kHz mono PCM of `text` spoken by macOS `say`.
    fn say(text: &str) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("touchless-live-{}.wav", std::process::id()));
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

    fn live_key() -> String {
        std::env::var("ELEVENLABS_API_KEY").expect("set ELEVENLABS_API_KEY to run live tests")
    }

    fn into_chunks(pcm: &[u8]) -> Vec<Vec<u8>> {
        pcm.chunks(CHUNK_SAMPLES * 2).map(<[u8]>::to_vec).collect()
    }

    async fn live_dictation(real_time: bool) {
        let mut pcm = say("The quick brown fox jumps over the lazy dog.");
        pcm.extend(silence(20).concat());
        pcm.extend(say("Final words are pineapple and submarine."));

        let mut client = ElevenLabs::connect(&live_key(), RATE, &TranscriptionOptions::default())
            .await
            .unwrap();
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let chunks = into_chunks(&pcm);
        let feeder = tokio::spawn(async move {
            for chunk in chunks {
                if real_time {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                audio_tx.send(chunk).unwrap();
            }
        });

        let mut texts = Vec::new();
        client
            .transcribe(audio_rx, &mut |event| {
                if let TranscriptEvent::Committed(text) = event {
                    texts.push(text);
                }
            })
            .await
            .unwrap();
        feeder.await.unwrap();
        let _ = client.socket.close(None).await;

        eprintln!(
            "live ({}): {} sent, {} answered: {texts:?}",
            if real_time { "real time" } else { "burst" },
            client.commits_sent,
            client.commits_answered
        );
        assert_eq!(client.commits_sent, 2);
        assert_eq!(client.commits_answered, client.commits_sent);
        let joined = texts.join(" ").to_lowercase();
        assert!(joined.contains("fox"), "{texts:?}");
        assert!(joined.contains("submarine"), "{texts:?}");
    }

    #[tokio::test]
    #[ignore = "streams to the real ElevenLabs API"]
    async fn live_answers_every_commit_in_real_time() {
        live_dictation(true).await;
    }

    #[tokio::test]
    #[ignore = "streams to the real ElevenLabs API"]
    async fn live_answers_every_commit_after_a_backlog() {
        live_dictation(false).await;
    }

    #[tokio::test]
    #[ignore = "streams to the real ElevenLabs API"]
    async fn live_answers_empty_commits() {
        let mut client = ElevenLabs::connect(&live_key(), RATE, &TranscriptionOptions::default())
            .await
            .unwrap();
        let mut pending = String::new();
        let mut texts = Vec::new();
        let audio = [into_chunks(&say("Hello there.")), silence(20)];
        for (index, chunks) in audio.iter().enumerate() {
            for chunk in chunks {
                client.send_chunk(chunk, false).await.unwrap();
            }
            client.send_chunk(&[], true).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while client.commits_answered <= index {
                let message = tokio::time::timeout_at(deadline, client.socket.next())
                    .await
                    .expect("commit answered in time");
                let text = client.handle(message).await.unwrap().expect("socket open");
                client
                    .dispatch(&text, &mut pending, &mut |event| {
                        if let TranscriptEvent::Committed(text) = event {
                            texts.push(text);
                        }
                    })
                    .unwrap();
            }
        }
        let _ = client.socket.close(None).await;
        eprintln!("live (empty commit): {texts:?}");
        assert_eq!(texts.len(), 2);
        assert!(texts[1].trim().is_empty(), "{texts:?}");
    }
}
