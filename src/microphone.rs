use std::{error::Error, io};

use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tokio::sync::mpsc;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const ELEVENLABS_SAMPLE_RATES: [u32; 6] = [16_000, 48_000, 44_100, 24_000, 22_050, 8_000];
const CHUNK_MILLISECONDS: usize = 200;

pub struct Microphone {
    stream: Stream,
    sample_rate: u32,
}

impl Microphone {
    pub fn open(audio: mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no input device found"))?;

        let config = device.default_input_config().map_err(|error| {
            io::Error::other(format!("could not read microphone format: {error}"))
        })?;

        if !ELEVENLABS_SAMPLE_RATES.contains(&config.sample_rate()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the default microphone sample rate ({}) is not supported by ElevenLabs",
                    config.sample_rate()
                ),
            )
            .into());
        }

        if !matches!(
            config.sample_format(),
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the default microphone sample format ({}) is not supported",
                    config.sample_format()
                ),
            )
            .into());
        }

        let sample_rate = config.sample_rate();
        let channels = config.channels() as usize;
        let stream_config = config.config();
        let chunk_bytes = sample_rate as usize * 2 * CHUNK_MILLISECONDS / 1_000;

        eprintln!(
            "Microphone: {} ({channels} channel(s), {sample_rate} Hz, {})",
            device,
            config.sample_format()
        );

        let stream = match config.sample_format() {
            SampleFormat::F32 => {
                build_stream::<f32>(&device, stream_config, channels, chunk_bytes, audio)?
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, stream_config, channels, chunk_bytes, audio)?
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, stream_config, channels, chunk_bytes, audio)?
            }
            _ => unreachable!("unsupported formats were filtered out"),
        };

        Ok(Self {
            stream,
            sample_rate,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn start(&self) -> Result<()> {
        self.stream
            .play()
            .map_err(|error| io::Error::other(format!("could not start microphone: {error}")))?;
        Ok(())
    }
}

fn build_stream<T>(
    device: &Device,
    config: StreamConfig,
    channels: usize,
    chunk_bytes: usize,
    audio: mpsc::Sender<Vec<u8>>,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    i16: FromSample<T>,
{
    let mut pending = Vec::with_capacity(chunk_bytes * 2);
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                for frame in data.chunks_exact(channels) {
                    let mono = (frame
                        .iter()
                        .map(|&sample| i16::from_sample(sample) as i64)
                        .sum::<i64>()
                        / channels as i64) as i16;
                    pending.extend_from_slice(&mono.to_le_bytes());

                    if pending.len() >= chunk_bytes {
                        let remainder = pending.split_off(chunk_bytes);
                        let chunk = std::mem::replace(&mut pending, remainder);
                        let _ = audio.try_send(chunk);
                    }
                }
            },
            |error| eprintln!("microphone error: {error}"),
            None,
        )
        .map_err(|error| io::Error::other(format!("could not open microphone: {error}")))?;

    Ok(stream)
}
