//! Short sounds when dictation starts and stops. The stop sound is the start
//! sound played backwards.

use tauri::AppHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    Start,
    Stop,
}

pub struct Sounds {
    app: AppHandle,
}

impl Sounds {
    /// Must run on the main thread (e.g. from `setup`).
    pub fn new(app: &AppHandle) -> Self {
        if let Err(error) = native::load() {
            eprintln!("could not load the dictation sounds: {error}");
        }
        Self { app: app.clone() }
    }

    pub fn play(&self, cue: Cue) {
        let _ = self.app.run_on_main_thread(move || native::play(cue));
    }
}

/// Peaks below this fraction of full scale (-60 dBFS) count as silence.
const SILENCE: f64 = 0.001;

/// Returns an uncompressed AIFF file played backwards. The quiet tail is cut
/// first; otherwise the reversed sound would open with that silence.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn reverse_aiff(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 12 || &bytes[..4] != b"FORM" {
        return Err("not an AIFF file".into());
    }
    if &bytes[8..12] != b"AIFF" {
        return Err("only uncompressed AIFF files are supported".into());
    }

    let mut comm = None;
    let mut sound = None;
    let mut rest = &bytes[12..];
    while rest.len() >= 8 {
        let size = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let body = rest.get(8..8 + size).ok_or("the AIFF file is truncated")?;
        match &rest[..4] {
            b"COMM" => comm = Some(body),
            b"SSND" => sound = Some(body),
            _ => {}
        }
        // Chunks are padded to an even length.
        rest = rest.get(8 + size + size % 2..).unwrap_or_default();
    }
    let comm = comm
        .filter(|comm| comm.len() >= 18)
        .ok_or("the AIFF file has no COMM chunk")?;
    let sound = sound
        .filter(|sound| sound.len() >= 8)
        .ok_or("the AIFF file has no SSND chunk")?;

    let channels = i16::from_be_bytes([comm[0], comm[1]]);
    let bits = i16::from_be_bytes([comm[6], comm[7]]);
    if channels < 1 || !(1..=32).contains(&bits) {
        return Err(format!(
            "unsupported AIFF format ({channels} channels, {bits} bits)"
        ));
    }
    let sample_bytes = (bits as usize).div_ceil(8);
    let frame_bytes = channels as usize * sample_bytes;
    let offset = u32::from_be_bytes([sound[0], sound[1], sound[2], sound[3]]) as usize;
    let data = sound
        .get(8 + offset..)
        .ok_or("the AIFF sound data is truncated")?;

    let frames: Vec<&[u8]> = data.chunks_exact(frame_bytes).collect();
    let audible = frames
        .iter()
        .rposition(|frame| {
            frame
                .chunks_exact(sample_bytes)
                .any(|sample| amplitude(sample) >= SILENCE)
        })
        .map(|last| last + 1)
        .ok_or("the sound is silent")?;

    let mut comm = comm[..18].to_vec();
    comm[2..6].copy_from_slice(&(audible as u32).to_be_bytes());
    // Offset and block size, both zero.
    let mut sound = vec![0; 8];
    for frame in frames[..audible].iter().rev() {
        sound.extend_from_slice(frame);
    }

    let mut out = b"FORM\0\0\0\0AIFF".to_vec();
    push_chunk(&mut out, b"COMM", &comm);
    push_chunk(&mut out, b"SSND", &sound);
    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// The magnitude of a big-endian signed sample, from 0 to 1.
fn amplitude(sample: &[u8]) -> f64 {
    let sign = if sample[0] & 0x80 != 0 { -1 } else { 0 };
    let value = sample
        .iter()
        .fold(sign, |value: i64, &byte| (value << 8) | i64::from(byte));
    value.unsigned_abs() as f64 / (1u64 << (sample.len() * 8 - 1)) as f64
}

#[cfg(target_os = "macos")]
mod native {
    use std::cell::RefCell;

    use objc2::{AllocAnyThread, rc::Retained};
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;

    use super::Cue;

    /// A short macOS system sound; read at runtime so it is not redistributed.
    pub const SOURCE: &str = "/System/Library/Sounds/Tink.aiff";

    struct Loaded {
        start: Retained<NSSound>,
        stop: Retained<NSSound>,
    }

    thread_local! {
        // NSSound is not thread-safe, so the sounds live on the main thread.
        static LOADED: RefCell<Option<Loaded>> = const { RefCell::new(None) };
    }

    pub fn load() -> Result<(), String> {
        let start = std::fs::read(SOURCE).map_err(|error| format!("{SOURCE}: {error}"))?;
        let stop = super::reverse_aiff(&start)?;
        let loaded = Loaded {
            start: sound(&start)?,
            stop: sound(&stop)?,
        };
        LOADED.with(|cell| *cell.borrow_mut() = Some(loaded));
        Ok(())
    }

    pub fn play(cue: Cue) {
        LOADED.with(|cell| {
            if let Some(loaded) = &*cell.borrow() {
                let sound = match cue {
                    Cue::Start => &loaded.start,
                    Cue::Stop => &loaded.stop,
                };
                // `play` does nothing while the sound is still playing.
                sound.stop();
                sound.play();
            }
        });
    }

    pub fn sound(bytes: &[u8]) -> Result<Retained<NSSound>, String> {
        NSSound::initWithData(NSSound::alloc(), &NSData::with_bytes(bytes))
            .ok_or_else(|| "macOS could not decode the sound".into())
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::Cue;

    pub fn load() -> Result<(), String> {
        Ok(())
    }

    pub fn play(_cue: Cue) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aiff(channels: i16, bits: i16, data: &[u8]) -> Vec<u8> {
        aiff_with_marker(channels, bits, data, false)
    }

    fn aiff_with_marker(channels: i16, bits: i16, data: &[u8], marker: bool) -> Vec<u8> {
        let frame_bytes = channels as usize * (bits as usize).div_ceil(8);
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&((data.len() / frame_bytes) as u32).to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        // 44.1 kHz as an 80-bit extended float.
        comm.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        let mut sound = vec![0; 8];
        sound.extend_from_slice(data);

        let mut out = b"FORM\0\0\0\0AIFF".to_vec();
        push_chunk(&mut out, b"COMM", &comm);
        if marker {
            // Odd-sized, so the reader has to skip a pad byte.
            push_chunk(&mut out, b"MARK", &[0, 0, 7]);
        }
        push_chunk(&mut out, b"SSND", &sound);
        let form_size = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&form_size.to_be_bytes());
        out
    }

    fn samples16(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_be_bytes())
            .collect()
    }

    #[test]
    fn reverses_frames_and_trims_the_quiet_tail() {
        let input = aiff_with_marker(1, 16, &samples16(&[1000, -2000, 3000, 5, 0, -3]), true);
        let output = reverse_aiff(&input).unwrap();
        assert_eq!(output, aiff(1, 16, &samples16(&[3000, -2000, 1000])));
    }

    #[test]
    fn keeps_stereo_24_bit_frames_intact() {
        let data = [
            0x10, 0x00, 0x00, 0xF0, 0x00, 0x00, // frame 1
            0x20, 0x00, 0x00, 0x00, 0x00, 0x00, // frame 2
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // silent frame
        ];
        let output = reverse_aiff(&aiff(2, 24, &data)).unwrap();
        assert_eq!(output, aiff(2, 24, &[&data[6..12], &data[..6]].concat()));
    }

    #[test]
    fn rejects_unsupported_files() {
        assert!(reverse_aiff(b"RIFF\0\0\0\0WAVE").is_err());
        let mut compressed = aiff(1, 16, &samples16(&[1000]));
        compressed[8..12].copy_from_slice(b"AIFC");
        assert!(reverse_aiff(&compressed).is_err());
        assert!(reverse_aiff(&aiff(1, 16, &samples16(&[0, 1]))).is_err());
        let truncated = aiff(1, 16, &samples16(&[1000, 2000]));
        assert!(reverse_aiff(&truncated[..truncated.len() - 3]).is_err());
    }

    #[test]
    fn measures_amplitude() {
        assert_eq!(amplitude(&[0x80, 0x00]), 1.0);
        assert_eq!(amplitude(&[0x40, 0x00]), 0.5);
        assert_eq!(amplitude(&[0xC0, 0x00, 0x00]), 0.5);
        assert_eq!(amplitude(&[0x00]), 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn loads_the_system_sound_both_ways() {
        let start = std::fs::read(native::SOURCE).unwrap();
        let stop = reverse_aiff(&start).unwrap();
        assert!(stop.len() < start.len() / 4, "the quiet tail should be cut");
        assert!(native::sound(&start).is_ok());
        assert!(native::sound(&stop).is_ok());
    }
}
