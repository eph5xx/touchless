use std::{error::Error, io, time::Duration};

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
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for waiting on the final transcript after the user stops.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for waiting on outstanding `edited_transcript` events after the
/// user stops, when an edit instruction is set.
const EDIT_TIMEOUT: Duration = Duration::from_secs(10);
/// With VAD commits, a transcript for earlier speech can arrive just before the
/// one for our manual commit, so keep listening briefly after each commit.
const SETTLE_WINDOW: Duration = Duration::from_millis(300);

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
            .append_pair("commit_strategy", "vad")
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
        if !options.transcript_edit.is_empty() {
            query.append_pair("transcript_edit", &options.transcript_edit);
        }
    }
    Ok(url)
}

pub struct ElevenLabs {
    socket: Socket,
    sample_rate: u32,
    finalize_timeout: Duration,
    edit_timeout: Duration,
    settle_window: Duration,
    editing: bool,
    /// Committed texts whose `edited_transcript` has not arrived yet.
    awaiting_edits: Vec<String>,
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
            settle_window: SETTLE_WINDOW,
            editing: !options.transcript_edit.is_empty(),
            awaiting_edits: Vec::new(),
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

    /// Streams audio until the channel closes, then commits whatever audio is still
    /// uncommitted and waits for its transcript. The latest partial transcript is
    /// reported as committed if the server never confirms it, so no speech is lost.
    pub async fn run(
        mut self,
        audio: mpsc::Receiver<Vec<u8>>,
        mut on_event: impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let mut pending = String::new();
        let result = self.stream(audio, &mut pending, &mut on_event).await;
        if !pending.is_empty() {
            on_event(TranscriptEvent::Committed(std::mem::take(&mut pending)));
        }
        let _ = self.socket.close(None).await;
        result
    }

    async fn stream(
        &mut self,
        mut audio: mpsc::Receiver<Vec<u8>>,
        pending: &mut String,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let mut sent_audio = false;
        loop {
            tokio::select! {
                chunk = audio.recv() => {
                    let Some(chunk) = chunk else { break };
                    if chunk.is_empty() { continue }
                    self.send_chunk(&chunk, false).await?;
                    sent_audio = true;
                }
                message = self.socket.next() => {
                    let Some(text) = self.handle(message).await? else { return Ok(()) };
                    self.dispatch(&text, pending, on_event)?;
                }
            }
        }

        if sent_audio {
            // Finalization is best effort: e.g. a commit for a very short clip can be
            // rejected, and the pending partial transcript is still a good result.
            if let Err(error) = self.finalize(pending, on_event).await {
                eprintln!("ElevenLabs finalization: {error}");
            }
        }
        Ok(())
    }

    async fn finalize(
        &mut self,
        pending: &mut String,
        on_event: &mut impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        self.send_chunk(&[], true).await?;

        let started = Instant::now();
        let hard_deadline = started + self.finalize_timeout;
        let edit_deadline = started + self.edit_timeout;
        let mut settle_deadline: Option<Instant> = None;
        loop {
            let commits_deadline =
                settle_deadline.map_or(hard_deadline, |settle| settle.min(hard_deadline));
            let deadline = if self.awaiting_edits.is_empty() {
                commits_deadline
            } else {
                commits_deadline.max(edit_deadline)
            };
            let Ok(message) = tokio::time::timeout_at(deadline, self.socket.next()).await else {
                return Ok(());
            };
            let Some(text) = self.handle(message).await? else {
                return Ok(());
            };
            match self.dispatch(&text, pending, on_event)? {
                ServerMessage::Committed(_) if pending.is_empty() => {
                    settle_deadline = Some(Instant::now() + self.settle_window);
                }
                ServerMessage::Partial(text) if !text.is_empty() => settle_deadline = None,
                _ => {}
            }
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
    ) -> Result<ServerMessage> {
        let message = parse_server_message(text);
        match &message {
            ServerMessage::Partial(text) => {
                pending.clone_from(text);
                on_event(TranscriptEvent::Partial(text.clone()));
            }
            ServerMessage::Committed(text) => {
                pending.clear();
                if self.editing && !text.trim().is_empty() {
                    self.awaiting_edits.push(text.clone());
                }
                on_event(TranscriptEvent::Committed(text.clone()));
            }
            ServerMessage::Edited { text, edited } => {
                // Edits may arrive out of commit order, so match them by text.
                if let Some(index) = self.awaiting_edits.iter().position(|item| item == text) {
                    self.awaiting_edits.remove(index);
                }
                on_event(TranscriptEvent::Edited {
                    original: text.clone(),
                    edited: edited.clone(),
                });
            }
            ServerMessage::Language(code) if !code.trim().is_empty() => {
                on_event(TranscriptEvent::Language(code.trim().to_owned()));
            }
            ServerMessage::Error(error) => return Err(io::Error::other(error.clone()).into()),
            ServerMessage::SessionStarted(_)
            | ServerMessage::Language(_)
            | ServerMessage::Other => {}
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "wss://api.elevenlabs.io/v1/speech-to-text/realtime?model_id=scribe_v2_realtime&audio_format=pcm_48000&commit_strategy=vad&include_language_detection=true"
        );
    }

    #[test]
    fn builds_url_with_all_options() {
        let options = TranscriptionOptions {
            language_code: Some("eng".into()),
            secondary_languages: vec!["rus".into(), "ukr".into()],
            no_verbatim: true,
            keyterms: vec!["Touchless".into(), "Scribe v2 & co".into()],
            transcript_edit: "Fix grammar.".into(),
        };
        let url = realtime_url(REALTIME_URL, 16_000, &options).unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let expected = [
            ("model_id", "scribe_v2_realtime"),
            ("audio_format", "pcm_16000"),
            ("commit_strategy", "vad"),
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

    /// Fake realtime server: for every received audio message, `respond` returns the
    /// JSON replies to send (given whether that message was a commit).
    async fn fake_server(
        respond: impl Fn(bool) -> Vec<(u64, &'static str)> + Send + 'static,
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

    async fn transcribe(url: &str, chunks: usize, finalize_timeout: Duration) -> Vec<String> {
        let events = transcribe_events(
            url,
            chunks,
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
        chunks: usize,
        options: &TranscriptionOptions,
        finalize_timeout: Duration,
        edit_timeout: Duration,
    ) -> Vec<TranscriptEvent> {
        let mut client = ElevenLabs::connect_to(url, "key", 16_000, options)
            .await
            .unwrap();
        client.finalize_timeout = finalize_timeout;
        client.edit_timeout = edit_timeout;
        let (audio_tx, audio_rx) = mpsc::channel(8);
        for _ in 0..chunks {
            audio_tx.send(vec![0; 320]).await.unwrap();
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
            transcript_edit: "Fix punctuation.".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn waits_for_edits_that_arrive_after_the_settle_window() {
        let url = fake_server(|commit| {
            if commit {
                vec![
                    (
                        10,
                        r#"{"message_type":"committed_transcript","text":"hi there"}"#,
                    ),
                    (
                        600,
                        r#"{"message_type":"edited_transcript","text":"hi there","edited_text":"Hi there."}"#,
                    ),
                ]
            } else {
                vec![]
            }
        })
        .await;

        let events = transcribe_events(&url, 1, &editing(), FINALIZE_TIMEOUT, EDIT_TIMEOUT).await;
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
        let url = fake_server(|commit| {
            if commit {
                vec![
                    (
                        10,
                        r#"{"message_type":"committed_transcript","text":"one"}"#,
                    ),
                    (
                        10,
                        r#"{"message_type":"committed_transcript","text":"two"}"#,
                    ),
                    (
                        10,
                        r#"{"message_type":"edited_transcript","text":"two","edited_text":"Two."}"#,
                    ),
                    (
                        10,
                        r#"{"message_type":"edited_transcript","text":"one","edited_text":"One."}"#,
                    ),
                ]
            } else {
                vec![]
            }
        })
        .await;

        let started = Instant::now();
        let events = transcribe_events(
            &url,
            1,
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
        let url = fake_server(|commit| {
            if commit {
                vec![(
                    10,
                    r#"{"message_type":"committed_transcript","text":"raw text"}"#,
                )]
            } else {
                vec![]
            }
        })
        .await;

        let events = transcribe_events(
            &url,
            1,
            &editing(),
            FINALIZE_TIMEOUT,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(events, [TranscriptEvent::Committed("raw text".into())]);
    }

    #[tokio::test]
    async fn commits_short_clip_before_any_partial_arrived() {
        let url = fake_server(|commit| {
            if commit {
                vec![(
                    50,
                    r#"{"message_type":"committed_transcript","text":"hi there"}"#,
                )]
            } else {
                vec![]
            }
        })
        .await;

        let committed = transcribe(&url, 2, FINALIZE_TIMEOUT).await;
        assert_eq!(committed, ["hi there"]);
    }

    #[tokio::test]
    async fn waits_for_late_commits_within_settle_window() {
        let url = fake_server(|commit| {
            if commit {
                vec![
                    (
                        10,
                        r#"{"message_type":"committed_transcript","text":"first"}"#,
                    ),
                    (
                        100,
                        r#"{"message_type":"committed_transcript","text":"second"}"#,
                    ),
                ]
            } else {
                vec![]
            }
        })
        .await;

        let committed = transcribe(&url, 1, FINALIZE_TIMEOUT).await;
        assert_eq!(committed, ["first", "second"]);
    }

    #[tokio::test]
    async fn falls_back_to_partial_when_commit_never_arrives() {
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

        let committed = transcribe(&url, 1, Duration::from_millis(300)).await;
        assert_eq!(committed, ["almost"]);
    }
}
