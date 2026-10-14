use std::{error::Error, io, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use tokio::{sync::mpsc, time::Instant};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const REALTIME_URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for waiting on the final transcript after the user stops.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
/// With VAD commits, a transcript for earlier speech can arrive just before the
/// one for our manual commit, so keep listening briefly after each commit.
const SETTLE_WINDOW: Duration = Duration::from_millis(300);

#[derive(Debug, PartialEq)]
pub enum TranscriptEvent {
    Partial(String),
    Committed(String),
}

#[derive(Debug, PartialEq)]
enum ServerMessage {
    SessionStarted,
    Partial(String),
    Committed(String),
    Error(String),
    Other,
}

fn parse_server_message(text: &str) -> ServerMessage {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return ServerMessage::Other;
    };
    let message_type = value["message_type"].as_str().unwrap_or_default();
    let text = || value["text"].as_str().unwrap_or_default().to_owned();

    match message_type {
        "session_started" => ServerMessage::SessionStarted,
        "partial_transcript" => ServerMessage::Partial(text()),
        "committed_transcript" => ServerMessage::Committed(text()),
        _ => match value["error"].as_str() {
            Some(error) => ServerMessage::Error(format!("{message_type}: {error}")),
            None => ServerMessage::Other,
        },
    }
}

pub struct ElevenLabs {
    socket: Socket,
    sample_rate: u32,
    finalize_timeout: Duration,
    settle_window: Duration,
}

impl ElevenLabs {
    pub async fn connect(api_key: &str, sample_rate: u32) -> Result<Self> {
        Self::connect_to(REALTIME_URL, api_key, sample_rate).await
    }

    async fn connect_to(base_url: &str, api_key: &str, sample_rate: u32) -> Result<Self> {
        let url = format!(
            "{base_url}?model_id=scribe_v2_realtime&audio_format=pcm_{sample_rate}&commit_strategy=vad"
        );
        let mut request = url.into_client_request()?;
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
            settle_window: SETTLE_WINDOW,
        })
    }

    /// Opens a session and waits for the server to accept it.
    pub async fn verify(api_key: &str) -> Result<()> {
        let mut client = Self::connect(api_key, 16_000).await?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let result = loop {
            let Ok(message) = tokio::time::timeout_at(deadline, client.socket.next()).await else {
                break Err("ElevenLabs did not respond".into());
            };
            let Some(text) = client.handle(message).await? else {
                break Err("ElevenLabs closed the connection".into());
            };
            match parse_server_message(&text) {
                ServerMessage::SessionStarted => break Ok(()),
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

        let hard_deadline = Instant::now() + self.finalize_timeout;
        let mut settle_deadline: Option<Instant> = None;
        loop {
            let deadline =
                settle_deadline.map_or(hard_deadline, |settle| settle.min(hard_deadline));
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
        &self,
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
                on_event(TranscriptEvent::Committed(text.clone()));
            }
            ServerMessage::Error(error) => return Err(io::Error::other(error.clone()).into()),
            ServerMessage::SessionStarted | ServerMessage::Other => {}
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
            parse_server_message(r#"{"message_type":"session_started","session_id":"x"}"#),
            ServerMessage::SessionStarted
        );
        assert_eq!(parse_server_message("not json"), ServerMessage::Other);
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
        let mut client = ElevenLabs::connect_to(url, "key", 16_000).await.unwrap();
        client.finalize_timeout = finalize_timeout;
        let (audio_tx, audio_rx) = mpsc::channel(8);
        for _ in 0..chunks {
            audio_tx.send(vec![0; 320]).await.unwrap();
        }
        drop(audio_tx);

        let mut committed = Vec::new();
        client
            .run(audio_rx, |event| {
                if let TranscriptEvent::Committed(text) = event {
                    committed.push(text);
                }
            })
            .await
            .unwrap();
        committed
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
