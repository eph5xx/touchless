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

const FINAL_COMMIT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq)]
pub enum TranscriptEvent {
    Partial(String),
    Committed(String),
}

#[derive(Debug, PartialEq)]
enum ServerMessage {
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
}

impl ElevenLabs {
    pub async fn connect(api_key: &str, sample_rate: u32) -> Result<Self> {
        let url = format!(
            "wss://api.elevenlabs.io/v1/speech-to-text/realtime\
             ?model_id=scribe_v2_realtime&audio_format=pcm_{sample_rate}&commit_strategy=vad"
        );
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .insert("xi-api-key", HeaderValue::from_str(api_key)?);

        let (socket, _) = connect_async(request).await?;
        Ok(Self {
            socket,
            sample_rate,
        })
    }

    /// Streams audio until the channel closes, then commits any pending speech and
    /// waits briefly for its final transcript before closing the socket.
    pub async fn run(
        mut self,
        mut audio: mpsc::Receiver<Vec<u8>>,
        mut on_event: impl FnMut(TranscriptEvent),
    ) -> Result<()> {
        let mut pending = String::new();

        loop {
            tokio::select! {
                chunk = audio.recv() => {
                    let Some(chunk) = chunk else { break };
                    self.send_chunk(&chunk, false).await?;
                }
                message = self.socket.next() => {
                    let Some(text) = self.handle(message).await? else {
                        if pending.is_empty() { return Ok(()) }
                        on_event(TranscriptEvent::Committed(std::mem::take(&mut pending)));
                        return Ok(());
                    };
                    self.dispatch(&text, &mut pending, &mut on_event)?;
                }
            }
        }

        if !pending.is_empty() {
            self.send_chunk(&[], true).await?;
            let deadline = Instant::now() + FINAL_COMMIT_TIMEOUT;
            while !pending.is_empty() {
                let Ok(message) = tokio::time::timeout_at(deadline, self.socket.next()).await
                else {
                    break;
                };
                let Some(text) = self.handle(message).await? else {
                    break;
                };
                self.dispatch(&text, &mut pending, &mut on_event)?;
            }
            if !pending.is_empty() {
                on_event(TranscriptEvent::Committed(std::mem::take(&mut pending)));
            }
        }

        let _ = self.socket.close(None).await;
        Ok(())
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
    ) -> Result<()> {
        match parse_server_message(text) {
            ServerMessage::Partial(text) => {
                *pending = text.clone();
                on_event(TranscriptEvent::Partial(text));
            }
            ServerMessage::Committed(text) => {
                pending.clear();
                on_event(TranscriptEvent::Committed(text));
            }
            ServerMessage::Error(error) => return Err(io::Error::other(error).into()),
            ServerMessage::Other => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ignores_other_messages() {
        assert_eq!(
            parse_server_message(r#"{"message_type":"session_started","session_id":"x"}"#),
            ServerMessage::Other
        );
        assert_eq!(parse_server_message("not json"), ServerMessage::Other);
    }
}
