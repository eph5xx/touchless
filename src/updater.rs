//! Finds new releases on GitHub and installs them.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{
    recorder::{Recorder, Status},
    settings::Settings,
    tray,
};

const RELEASES_URL: &str = "https://github.com/eph5xx/touchless/releases";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The monotonic clock stops while the Mac sleeps, so the background loop
/// wakes often and compares wall-clock time instead of sleeping six hours.
const CHECK_TICK: Duration = Duration::from_secs(15 * 60);
const IDLE_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
        notes: Option<String>,
        date: Option<String>,
    },
    Downloading {
        version: String,
        percent: Option<u8>,
    },
    /// Downloaded and verified; installs once no dictation is running.
    Ready {
        version: String,
    },
    Error {
        message: String,
    },
}

impl UpdateState {
    /// The version being offered or installed.
    pub fn new_version(&self) -> Option<&str> {
        match self {
            Self::Available { version, .. }
            | Self::Downloading { version, .. }
            | Self::Ready { version } => Some(version),
            _ => None,
        }
    }

    fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Ready { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateView {
    current_version: String,
    #[serde(flatten)]
    state: UpdateState,
}

struct Inner {
    state: UpdateState,
    update: Option<Update>,
}

pub struct Updater {
    app: AppHandle,
    inner: Mutex<Inner>,
}

impl Updater {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            inner: Mutex::new(Inner {
                state: UpdateState::Idle,
                update: None,
            }),
        }
    }

    pub fn view(&self) -> UpdateView {
        UpdateView {
            current_version: self.app.package_info().version.to_string(),
            state: self.inner.lock().unwrap().state.clone(),
        }
    }

    /// Asks GitHub for a newer release. Failed background checks keep the
    /// previous state, so being offline does not show an error.
    pub async fn check(&self, manual: bool) {
        let previous = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.is_busy() {
                return;
            }
            std::mem::replace(&mut inner.state, UpdateState::Checking)
        };
        self.emit();

        let result = match self.app.updater() {
            Ok(updater) => updater.check().await,
            Err(error) => Err(error),
        };
        self.change(|inner| match result {
            Ok(Some(update)) => {
                inner.state = UpdateState::Available {
                    version: update.version.clone(),
                    notes: update.body.clone().filter(|body| !body.trim().is_empty()),
                    date: update.raw_json["pub_date"].as_str().map(str::to_owned),
                };
                inner.update = Some(update);
            }
            Ok(None) => {
                inner.state = UpdateState::UpToDate;
                inner.update = None;
            }
            Err(error) if manual => {
                inner.state = UpdateState::Error {
                    message: format!("Could not check for updates: {error}"),
                };
            }
            Err(error) => {
                eprintln!("could not check for updates: {error}");
                inner.state = previous;
            }
        });
    }

    /// Downloads the update, waits for dictation to finish, installs it and restarts.
    pub fn install(self: &Arc<Self>) -> Result<(), String> {
        if cfg!(debug_assertions) {
            return Err("Development builds don't install updates.".into());
        }
        let update = {
            let mut inner = self.inner.lock().unwrap();
            let update = match (&inner.state, &inner.update) {
                (UpdateState::Available { .. }, Some(update)) => update.clone(),
                _ => return Err("There is no update to install. Check for updates first.".into()),
            };
            inner.state = UpdateState::Downloading {
                version: update.version.clone(),
                percent: None,
            };
            update
        };
        self.emit();

        let updater = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let Err(message) = updater.download_and_install(update).await;
            updater.change(|inner| inner.state = UpdateState::Error { message });
        });
        Ok(())
    }

    async fn download_and_install(&self, update: Update) -> Result<std::convert::Infallible, String> {
        let version = update.version.clone();
        let mut downloaded = 0u64;
        let mut reported = None;
        let bytes = update
            .download(
                |chunk, total| {
                    downloaded += chunk as u64;
                    let percent = total
                        .filter(|total| *total > 0)
                        .map(|total| (downloaded * 100 / total).min(100) as u8);
                    if percent != reported {
                        reported = percent;
                        self.change(|inner| {
                            inner.state = UpdateState::Downloading {
                                version: version.clone(),
                                percent,
                            }
                        });
                    }
                },
                || {},
            )
            .await
            .map_err(|error| format!("Could not download the update: {error}"))?;
        self.change(|inner| {
            inner.state = UpdateState::Ready {
                version: version.clone(),
            }
        });

        // Restarting mid-dictation would lose what the user is saying.
        let recorder = Arc::clone(&self.app.state::<Arc<Recorder>>());
        while recorder.snapshot().status != Status::Idle {
            tokio::time::sleep(IDLE_POLL).await;
        }

        update
            .install(&bytes)
            .map_err(|error| format!("Could not install the update: {error}"))?;
        self.app.restart()
    }

    pub fn open_release_page(&self) -> Result<(), String> {
        let url = match self.inner.lock().unwrap().state.new_version() {
            Some(version) => format!("{RELEASES_URL}/tag/v{version}"),
            None => format!("{RELEASES_URL}/latest"),
        };
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn change(&self, change: impl FnOnce(&mut Inner)) {
        change(&mut self.inner.lock().unwrap());
        self.emit();
    }

    fn emit(&self) {
        let view = self.view();
        tray::set_update_version(&self.app, view.state.new_version());
        let _ = self.app.emit("update-state", view);
    }
}

/// Checks soon after launch and then every few hours while the preference is on.
/// Development builds never check on their own.
pub fn start_background_checks(updater: Arc<Updater>) {
    if cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut last_check: Option<SystemTime> = None;
        loop {
            let due = last_check
                .and_then(|time| time.elapsed().ok())
                .is_none_or(|elapsed| elapsed >= CHECK_INTERVAL);
            let enabled = updater
                .app
                .state::<Settings>()
                .preferences()
                .check_for_updates;
            if due && enabled {
                updater.check(false).await;
                last_check = Some(SystemTime::now());
            }
            tokio::time::sleep(CHECK_TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_state_with_its_status() {
        let view = UpdateView {
            current_version: "0.1.0".into(),
            state: UpdateState::Downloading {
                version: "0.2.0".into(),
                percent: Some(42),
            },
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "current_version": "0.1.0",
                "status": "downloading",
                "version": "0.2.0",
                "percent": 42,
            })
        );
        assert_eq!(
            serde_json::to_value(UpdateState::UpToDate).unwrap(),
            serde_json::json!({ "status": "up_to_date" })
        );
    }

    #[test]
    fn reports_the_new_version() {
        assert_eq!(UpdateState::Idle.new_version(), None);
        let ready = UpdateState::Ready {
            version: "0.2.0".into(),
        };
        assert_eq!(ready.new_version(), Some("0.2.0"));
        assert!(ready.is_busy());
        assert!(!UpdateState::UpToDate.is_busy());
    }
}
