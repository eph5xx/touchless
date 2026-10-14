mod elevenlabs;
mod microphone;

use std::{env, io};

use elevenlabs::ElevenLabs;
use microphone::Microphone;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    dotenvy::dotenv().ok();

    let api_key = env::var("ELEVENLABS_API_KEY").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "set ELEVENLABS_API_KEY in the environment or in .env",
        )
    })?;

    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(32);
    let microphone = Microphone::open(audio_tx)?;
    let elevenlabs = ElevenLabs::connect(&api_key, microphone.sample_rate()).await?;

    microphone.start()?;
    eprintln!("Listening on the default microphone. Press Ctrl+C to stop.");

    tokio::select! {
        result = elevenlabs.run(audio_rx) => result?,
        result = tokio::signal::ctrl_c() => result?,
    }

    Ok(())
}
