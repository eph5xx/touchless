//! The Whisper models Touchless can run on this Mac, and their downloads.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{io::AsyncWriteExt, sync::Notify};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A stalled download fails after this long without data.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhisperModel {
    WhisperLargeV3Turbo,
    WhisperMedium,
    WhisperSmall,
}

pub struct ModelSpec {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

impl WhisperModel {
    pub const ALL: [Self; 3] = [
        Self::WhisperLargeV3Turbo,
        Self::WhisperMedium,
        Self::WhisperSmall,
    ];

    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::WhisperLargeV3Turbo => &ModelSpec {
                name: "Whisper Large v3 Turbo",
                file: "ggml-large-v3-turbo.bin",
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
                size: 1_624_555_275,
                sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
            },
            Self::WhisperMedium => &ModelSpec {
                name: "Whisper Medium",
                file: "ggml-medium.bin",
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
                size: 1_533_763_059,
                sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
            },
            Self::WhisperSmall => &ModelSpec {
                name: "Whisper Small",
                file: "ggml-small.bin",
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
                size: 487_601_967,
                sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
            },
        }
    }

    pub fn name(self) -> &'static str {
        self.spec().name
    }
}

/// A Whisper model on this Mac: one Touchless downloads, or a file another app did.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalModel {
    Builtin(WhisperModel),
    External(PathBuf),
}

impl LocalModel {
    pub fn name(&self) -> String {
        match self {
            Self::Builtin(model) => model.name().to_owned(),
            Self::External(path) => crate::external::file_name(path),
        }
    }
}

/// The built-in model's id, or the absolute path of an external file.
impl Serialize for LocalModel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Builtin(model) => model.serialize(serializer),
            Self::External(path) => serializer.serialize_str(&path.to_string_lossy()),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    NotDownloaded,
    Downloading,
    Downloaded,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelView {
    id: WhisperModel,
    name: &'static str,
    size: u64,
    state: ModelState,
    received: u64,
    total: u64,
}

#[derive(Debug, Clone, Serialize)]
struct Progress {
    id: WhisperModel,
    received: u64,
    total: u64,
}

struct Download {
    received: u64,
    total: u64,
    cancel: Arc<Notify>,
}

/// Model files live in one folder; a download writes `<file>.part` and is
/// renamed into place only once its size and SHA-256 match.
pub struct ModelStore {
    dir: PathBuf,
    downloads: Mutex<HashMap<WhisperModel, Download>>,
}

impl ModelStore {
    pub fn new(dir: PathBuf) -> Self {
        // Downloads don't resume, so leftovers from a previous run are useless.
        for model in WhisperModel::ALL {
            let _ = std::fs::remove_file(part_path(&dir.join(model.spec().file)));
        }
        Self {
            dir,
            downloads: Mutex::new(HashMap::new()),
        }
    }

    pub fn path(&self, model: WhisperModel) -> PathBuf {
        self.dir.join(model.spec().file)
    }

    /// Where `model` is read from.
    pub fn local_path(&self, model: &LocalModel) -> PathBuf {
        match model {
            LocalModel::Builtin(model) => self.path(*model),
            LocalModel::External(path) => path.clone(),
        }
    }

    pub fn is_downloaded(&self, model: WhisperModel) -> bool {
        std::fs::metadata(self.path(model))
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == model.spec().size)
    }

    pub fn view(&self) -> Vec<ModelView> {
        let downloads = self.downloads.lock().unwrap();
        WhisperModel::ALL
            .into_iter()
            .map(|model| {
                let spec = model.spec();
                let (state, received, total) = match downloads.get(&model) {
                    Some(download) => (ModelState::Downloading, download.received, download.total),
                    None if self.is_downloaded(model) => {
                        (ModelState::Downloaded, spec.size, spec.size)
                    }
                    None => (ModelState::NotDownloaded, 0, spec.size),
                };
                ModelView {
                    id: model,
                    name: spec.name,
                    size: spec.size,
                    state,
                    received,
                    total,
                }
            })
            .collect()
    }

    /// Starts downloading in the background; `on_done` runs after a successful download.
    pub fn download(
        &self,
        app: &AppHandle,
        model: WhisperModel,
        on_done: impl FnOnce(&AppHandle) + Send + 'static,
    ) -> Result<(), String> {
        if self.is_downloaded(model) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create the models folder: {error}"))?;
        let cancel = Arc::new(Notify::new());
        {
            let mut downloads = self.downloads.lock().unwrap();
            if downloads.contains_key(&model) {
                return Ok(());
            }
            downloads.insert(
                model,
                Download {
                    received: 0,
                    total: model.spec().size,
                    cancel: Arc::clone(&cancel),
                },
            );
        }
        self.changed(app);

        let app = app.clone();
        let dest = self.path(model);
        tauri::async_runtime::spawn(async move {
            let spec = model.spec();
            let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
            let result = fetch(
                spec.url,
                &dest,
                spec.size,
                spec.sha256,
                &cancel,
                |received, total| {
                    let store = app.state::<ModelStore>();
                    if let Some(download) = store.downloads.lock().unwrap().get_mut(&model) {
                        download.received = received;
                        download.total = total;
                    }
                    if last_emit.elapsed() >= PROGRESS_INTERVAL {
                        last_emit = Instant::now();
                        let _ = app.emit(
                            "model-download-progress",
                            Progress {
                                id: model,
                                received,
                                total,
                            },
                        );
                    }
                },
            )
            .await;

            let store = app.state::<ModelStore>();
            store.downloads.lock().unwrap().remove(&model);
            match result {
                Ok(()) => {
                    store.changed(&app);
                    on_done(&app);
                }
                Err(FetchError::Cancelled) => store.changed(&app),
                Err(FetchError::Failed(error)) => {
                    eprintln!("{} download failed: {error}", spec.name);
                    store.changed(&app);
                    let _ = app.emit(
                        "model-download-failed",
                        serde_json::json!({ "id": model, "error": error }),
                    );
                }
            }
        });
        Ok(())
    }

    pub fn cancel(&self, model: WhisperModel) {
        if let Some(download) = self.downloads.lock().unwrap().get(&model) {
            download.cancel.notify_one();
        }
    }

    pub fn delete(&self, model: WhisperModel) -> Result<(), String> {
        match std::fs::remove_file(self.path(model)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("could not delete {}: {error}", model.name()))
            }
            _ => Ok(()),
        }
    }

    pub fn changed(&self, app: &AppHandle) {
        let _ = app.emit("models-changed", self.view());
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

#[derive(Debug, PartialEq)]
enum FetchError {
    Cancelled,
    Failed(String),
}

/// Downloads `url` to `dest` through a `.part` file, checking its size and SHA-256.
/// `progress` gets the bytes received and the expected total.
async fn fetch(
    url: &str,
    dest: &Path,
    size: u64,
    sha256: &str,
    cancel: &Notify,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), FetchError> {
    let part = part_path(dest);
    let result = tokio::select! {
        result = fetch_to(url, &part, size, sha256, &mut progress) => result.map_err(FetchError::Failed),
        () = cancel.notified() => Err(FetchError::Cancelled),
    };
    match result {
        Ok(()) => tokio::fs::rename(&part, dest)
            .await
            .map_err(|error| FetchError::Failed(format!("could not save the model: {error}"))),
        Err(error) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(error)
        }
    }
}

async fn fetch_to(
    url: &str,
    part: &Path,
    size: u64,
    sha256: &str,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("could not start the download: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("the server responded with {}", response.status()));
    }

    let mut file = tokio::fs::File::create(part)
        .await
        .map_err(|error| format!("could not create the model file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut body = response.bytes_stream();
    progress(0, size);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| format!("the download was interrupted: {error}"))?;
        received += chunk.len() as u64;
        if received > size {
            return Err("the file is larger than expected".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("could not write the model file: {error}"))?;
        progress(received, size);
    }
    file.flush()
        .await
        .map_err(|error| format!("could not write the model file: {error}"))?;
    drop(file);

    if received != size {
        return Err(format!("expected {size} bytes but got {received}"));
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != sha256 {
        return Err("the file is damaged (checksum mismatch)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn catalog_entries_are_unique() {
        let files: HashSet<_> = WhisperModel::ALL.iter().map(|m| m.spec().file).collect();
        let urls: HashSet<_> = WhisperModel::ALL.iter().map(|m| m.spec().url).collect();
        assert_eq!(files.len(), WhisperModel::ALL.len());
        assert_eq!(urls.len(), WhisperModel::ALL.len());
        for model in WhisperModel::ALL {
            let spec = model.spec();
            assert!(spec.url.ends_with(spec.file));
            assert_eq!(spec.sha256.len(), 64);
        }
    }

    #[test]
    fn serializes_model_ids() {
        assert_eq!(
            serde_json::to_string(&WhisperModel::WhisperLargeV3Turbo).unwrap(),
            r#""whisper_large_v3_turbo""#
        );
        assert_eq!(
            serde_json::to_string(&LocalModel::Builtin(WhisperModel::WhisperSmall)).unwrap(),
            r#""whisper_small""#
        );
        assert_eq!(
            serde_json::to_string(&LocalModel::External("/m/ggml-base.bin".into())).unwrap(),
            r#""/m/ggml-base.bin""#
        );
    }

    #[test]
    fn downloaded_means_the_full_file_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().to_owned());
        let model = WhisperModel::WhisperSmall;
        assert!(!store.is_downloaded(model));
        let file = std::fs::File::create(store.path(model)).unwrap();
        file.set_len(10).unwrap();
        assert!(!store.is_downloaded(model));
        file.set_len(model.spec().size).unwrap();
        assert!(store.is_downloaded(model));
        store.delete(model).unwrap();
        assert!(!store.is_downloaded(model));
        store.delete(model).unwrap();
    }

    /// Serves `body` once; with `stall`, sends half of it and then waits.
    async fn fake_server(body: Vec<u8>, stall: bool) -> String {
        // reqwest needs a TLS backend even for plain HTTP; `main` installs it in the app.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            if stall {
                stream.write_all(&body[..body.len() / 2]).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
            } else {
                stream.write_all(&body).await.unwrap();
            }
        });
        format!("http://{address}/model.bin")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[tokio::test]
    async fn downloads_and_renames_a_verified_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        let body = b"hello whisper".repeat(1000);
        let url = fake_server(body.clone(), false).await;
        let mut last = (0, 0);
        fetch(
            &url,
            &dest,
            body.len() as u64,
            &sha256_hex(&body),
            &Notify::new(),
            |r, t| last = (r, t),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
        assert_eq!(last, (body.len() as u64, body.len() as u64));
    }

    #[tokio::test]
    async fn rejects_a_wrong_size_or_hash() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        let body = b"hello whisper".to_vec();

        let url = fake_server(body.clone(), false).await;
        let result = fetch(
            &url,
            &dest,
            5,
            &sha256_hex(&body),
            &Notify::new(),
            |_, _| {},
        )
        .await;
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        assert!(!dest.exists() && !part_path(&dest).exists());

        let url = fake_server(body.clone(), false).await;
        let result = fetch(
            &url,
            &dest,
            100,
            &sha256_hex(&body),
            &Notify::new(),
            |_, _| {},
        )
        .await;
        assert!(matches!(result, Err(FetchError::Failed(_))), "{result:?}");
        assert!(!dest.exists() && !part_path(&dest).exists());

        let url = fake_server(body.clone(), false).await;
        let result = fetch(
            &url,
            &dest,
            body.len() as u64,
            &"0".repeat(64),
            &Notify::new(),
            |_, _| {},
        )
        .await;
        assert_eq!(
            result,
            Err(FetchError::Failed(
                "the file is damaged (checksum mismatch)".into()
            ))
        );
        assert!(!dest.exists() && !part_path(&dest).exists());
    }

    #[tokio::test]
    async fn cancel_removes_the_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        let body = vec![7u8; 64 * 1024];
        let url = fake_server(body.clone(), true).await;
        let cancel = Arc::new(Notify::new());
        let canceller = Arc::clone(&cancel);
        let mut cancelled = false;
        let result = fetch(
            &url,
            &dest,
            body.len() as u64,
            &sha256_hex(&body),
            &cancel,
            |received, _| {
                if received > 0 && !cancelled {
                    cancelled = true;
                    assert!(part_path(&dest).exists());
                    canceller.notify_one();
                }
            },
        )
        .await;
        assert_eq!(result, Err(FetchError::Cancelled));
        assert!(!dest.exists() && !part_path(&dest).exists());
    }
}
