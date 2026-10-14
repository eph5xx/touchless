use std::{
    error::Error,
    io::{self, Write},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

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

    pub async fn run(mut self, mut audio: mpsc::Receiver<Vec<u8>>) -> Result<()> {
        loop {
            tokio::select! {
                chunk = audio.recv() => {
                    let Some(chunk) = chunk else { break };
                    let message = serde_json::json!({
                        "message_type": "input_audio_chunk",
                        "audio_base_64": STANDARD.encode(chunk),
                        "sample_rate": self.sample_rate,
                    });
                    self.socket.send(Message::Text(message.to_string().into())).await?;
                }
                message = self.socket.next() => {
                    match message.transpose()? {
                        Some(Message::Text(text)) => {
                            println!("{text}");
                            io::stdout().flush()?;
                        }
                        Some(Message::Ping(data)) => {
                            self.socket.send(Message::Pong(data)).await?;
                        }
                        Some(Message::Close(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }
}
