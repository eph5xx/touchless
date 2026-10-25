//! whisper.cpp model files that other apps downloaded. Touchless reads them
//! where they are and never changes or deletes them.

use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

use serde::Serialize;

/// "ggml" as whisper.cpp writes it, a little-endian `u32`.
const GGML_MAGIC: u32 = 0x6767_6d6c;
/// The marker and the 11 `i32` settings that follow it.
const HEADER_LEN: usize = 48;
/// English-only models have one token fewer than multilingual ones.
const ENGLISH_VOCAB: i32 = 51_864;
const HANDY_MODELS: &str = "Library/Application Support/com.pais.handy/models";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub label: String,
    pub size: u64,
    pub english_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalModelView {
    /// The absolute path, which is also the engine id.
    id: String,
    name: String,
    label: Option<String>,
    /// The path with `~` for the home folder.
    path: String,
    size: Option<u64>,
    english_only: bool,
    missing: bool,
    error: Option<String>,
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn not_whisper(name: &str) -> String {
    format!("{name} isn't a whisper.cpp model.")
}

/// Checks that `path` is a whisper.cpp model and describes it from its header.
pub fn inspect(path: &Path) -> Result<ModelInfo, String> {
    let name = file_name(path);
    let unreadable = |error: std::io::Error| match error.kind() {
        ErrorKind::NotFound => format!("Touchless can't find {name}."),
        _ => format!("Touchless can't read {name}: {error}"),
    };
    let mut file = File::open(path).map_err(unreadable)?;
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(not_whisper(&name));
    }
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|error| match error.kind() {
            ErrorKind::UnexpectedEof => not_whisper(&name),
            _ => unreadable(error),
        })?;

    let (words, _) = header.as_chunks::<4>();
    let word = |index: usize| i32::from_le_bytes(words[index]);
    if word(0) as u32 != GGML_MAGIC {
        return Err(not_whisper(&name));
    }
    let (n_vocab, n_audio_layer, n_text_layer, n_mels) = (word(1), word(5), word(9), word(10));
    Ok(ModelInfo {
        label: label(n_audio_layer, n_text_layer, n_mels),
        size: metadata.len(),
        english_only: n_vocab == ENGLISH_VOCAB,
    })
}

fn label(n_audio_layer: i32, n_text_layer: i32, n_mels: i32) -> String {
    let size = match n_audio_layer {
        4 => "Tiny",
        6 => "Base",
        12 => "Small",
        24 => "Medium",
        32 => "Large",
        _ => return "Whisper model".into(),
    };
    let mut label = format!("Whisper {size}");
    if n_mels == 128 {
        label.push_str(" v3");
    }
    // Turbo is Large v3 with the text decoder cut down to 4 layers.
    if n_audio_layer == 32 && n_text_layer == 4 {
        label.push_str(" Turbo");
    }
    label
}

pub fn view(path: &Path) -> ExternalModelView {
    let (info, error) = match inspect(path) {
        Ok(info) => (Some(info), None),
        Err(error) => (None, Some(error)),
    };
    ExternalModelView {
        id: path.to_string_lossy().into_owned(),
        name: file_name(path),
        label: info.as_ref().map(|info| info.label.clone()),
        path: with_tilde(path),
        size: info.as_ref().map(|info| info.size),
        english_only: info.as_ref().is_some_and(|info| info.english_only),
        missing: !path.exists(),
        error,
    }
}

pub fn views(paths: &[PathBuf]) -> Vec<ExternalModelView> {
    paths.iter().map(|path| view(path)).collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn with_tilde(path: &Path) -> String {
    match home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Where the file picker starts: Handy's models folder, if it exists.
pub fn suggested_folder() -> Option<PathBuf> {
    home()
        .map(|home| home.join(HANDY_MODELS))
        .filter(|dir| dir.is_dir())
}

/// A whisper.cpp header with the given settings, padded like a real file.
#[cfg(test)]
pub fn test_header(n_vocab: i32, n_audio_layer: i32, n_text_layer: i32, n_mels: i32) -> Vec<u8> {
    let words = [
        GGML_MAGIC as i32,
        n_vocab,
        1500,
        768,
        12,
        n_audio_layer,
        448,
        768,
        12,
        n_text_layer,
        n_mels,
        1,
    ];
    let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    bytes.resize(1024, 0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn describes_a_multilingual_small_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "ggml-small.bin",
            &test_header(51_865, 12, 12, 80),
        );
        assert_eq!(
            inspect(&path).unwrap(),
            ModelInfo {
                label: "Whisper Small".into(),
                size: 1024,
                english_only: false,
            }
        );
    }

    #[test]
    fn describes_large_v3_turbo() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "turbo.bin", &test_header(51_866, 32, 4, 128));
        let info = inspect(&path).unwrap();
        assert_eq!(info.label, "Whisper Large v3 Turbo");
        assert!(!info.english_only);

        let path = write(dir.path(), "large.bin", &test_header(51_866, 32, 32, 128));
        assert_eq!(inspect(&path).unwrap().label, "Whisper Large v3");
    }

    #[test]
    fn flags_english_only_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "ggml-base.en.bin",
            &test_header(51_864, 6, 6, 80),
        );
        let info = inspect(&path).unwrap();
        assert_eq!(info.label, "Whisper Base");
        assert!(info.english_only);
    }

    #[test]
    fn rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = test_header(51_865, 12, 12, 80);
        bytes[..4].copy_from_slice(b"ORTM");
        let path = write(dir.path(), "model.ort", &bytes);
        assert_eq!(
            inspect(&path),
            Err("model.ort isn't a whisper.cpp model.".into())
        );

        let path = write(
            dir.path(),
            "short.bin",
            &test_header(51_865, 12, 12, 80)[..20],
        );
        assert_eq!(
            inspect(&path),
            Err("short.bin isn't a whisper.cpp model.".into())
        );

        assert_eq!(
            inspect(dir.path()),
            Err(format!(
                "{} isn't a whisper.cpp model.",
                file_name(dir.path())
            ))
        );
    }

    #[test]
    fn reports_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.bin");
        assert_eq!(inspect(&path), Err("Touchless can't find gone.bin.".into()));
        let view = view(&path);
        assert!(view.missing);
        assert_eq!(view.name, "gone.bin");
        assert_eq!(view.size, None);
    }
}
