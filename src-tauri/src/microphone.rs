use std::{error::Error, io};

use cpal::{
    Device, FromSample, Host, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::Serialize;
use tokio::sync::mpsc;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const CHUNK_MILLISECONDS: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct MicrophoneView {
    id: String,
    name: String,
    /// The Mac's current default input.
    default: bool,
}

/// Connected input devices; ones whose id or name can't be read are skipped.
pub fn list() -> Vec<MicrophoneView> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|device| {
            let id = device.id().ok()?.to_string();
            let name = device.description().ok()?.name().to_owned();
            Some(MicrophoneView {
                default: default_id.as_ref() == Some(&id),
                id,
                name,
            })
        })
        .collect()
}

/// The device with id `preferred`, or the default input when it isn't connected.
fn input_device(host: &Host, preferred: Option<&str>) -> Option<Device> {
    if let Some(preferred) = preferred {
        let found = host.input_devices().ok().and_then(|mut devices| {
            devices.find(|device| device.id().is_ok_and(|id| id.to_string() == preferred))
        });
        if found.is_some() {
            return found;
        }
        eprintln!("Microphone {preferred} not connected; using the default");
    }
    host.default_input_device()
}

pub struct Microphone {
    stream: Stream,
    sample_rate: u32,
}

impl Microphone {
    /// Opens the microphone with id `preferred`, falling back to the default input.
    pub fn open(audio: mpsc::UnboundedSender<Vec<u8>>, preferred: Option<&str>) -> Result<Self> {
        let device = input_device(&cpal::default_host(), preferred)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no input device found"))?;

        let config = device.default_input_config().map_err(|error| {
            io::Error::other(format!("could not read microphone format: {error}"))
        })?;

        if !matches!(
            config.sample_format(),
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "this microphone's sample format ({}) is not supported",
                    config.sample_format()
                ),
            )
            .into());
        }

        let sample_rate = config.sample_rate();
        let channels = config.channels() as usize;
        let stream_config = config.config();
        let chunker = Chunker::new(sample_rate as usize * 2 * CHUNK_MILLISECONDS / 1_000, audio);

        eprintln!(
            "Microphone: {} ({channels} channel(s), {sample_rate} Hz, {})",
            device,
            config.sample_format()
        );

        let stream = match config.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(&device, stream_config, channels, chunker)?,
            SampleFormat::I16 => build_stream::<i16>(&device, stream_config, channels, chunker)?,
            SampleFormat::U16 => build_stream::<u16>(&device, stream_config, channels, chunker)?,
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

/// Groups 16-bit mono PCM into fixed-size chunks. The trailing partial chunk is
/// sent when the chunker is dropped (i.e. when the stream closes), so the last
/// words before stopping are not lost.
struct Chunker {
    pending: Vec<u8>,
    chunk_bytes: usize,
    audio: mpsc::UnboundedSender<Vec<u8>>,
}

impl Chunker {
    fn new(chunk_bytes: usize, audio: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            pending: Vec::with_capacity(chunk_bytes * 2),
            chunk_bytes,
            audio,
        }
    }

    fn push(&mut self, sample: i16) {
        self.pending.extend_from_slice(&sample.to_le_bytes());
        if self.pending.len() >= self.chunk_bytes {
            let remainder = self.pending.split_off(self.chunk_bytes);
            let chunk = std::mem::replace(&mut self.pending, remainder);
            let _ = self.audio.send(chunk);
        }
    }
}

impl Drop for Chunker {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let _ = self.audio.send(std::mem::take(&mut self.pending));
        }
    }
}

fn build_stream<T>(
    device: &Device,
    config: StreamConfig,
    channels: usize,
    mut chunker: Chunker,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    i16: FromSample<T>,
{
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                for frame in data.chunks_exact(channels) {
                    let mono = frame
                        .iter()
                        .map(|&sample| i16::from_sample(sample) as i64)
                        .sum::<i64>()
                        / channels as i64;
                    chunker.push(mono as i16);
                }
            },
            |error| eprintln!("microphone error: {error}"),
            None,
        )
        .map_err(|error| io::Error::other(format!("could not open microphone: {error}")))?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_and_flushes_remainder_on_drop() {
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();
        let mut chunker = Chunker::new(4, audio_tx);
        for sample in 0..3 {
            chunker.push(sample);
        }
        assert_eq!(audio_rx.try_recv().unwrap(), [0, 0, 1, 0]);
        assert!(audio_rx.try_recv().is_err());

        drop(chunker);
        assert_eq!(audio_rx.try_recv().unwrap(), [2, 0]);
        assert!(audio_rx.try_recv().is_err());
    }
}
