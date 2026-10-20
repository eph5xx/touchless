//! The card at the top-right of the screen where plugin tips appear. Like the
//! pill it must never become key or activate Touchless, and clicks pass through.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::{overlay, plugin_api::Card};

const LABEL: &str = "tips";
const WIDTH: f64 = 360.0;
/// Room for a few stacked cards; the window is transparent and ignores the mouse.
const HEIGHT: f64 = 260.0;
const MARGIN: f64 = 12.0;
/// Matches the fade-out in tips.css, so the last card finishes fading first.
const FADE: Duration = Duration::from_millis(250);

#[derive(Clone, Serialize)]
struct TipShown<'a> {
    card: &'a Card,
    duration_ms: u64,
}

#[derive(Default)]
struct Visibility {
    /// Bumped on every show so an earlier scheduled hide does not cut a newer card short.
    generation: u64,
    /// When the last card on screen is gone; `None` while hidden.
    until: Option<Instant>,
}

pub struct Tips {
    app: AppHandle,
    visibility: Arc<Mutex<Visibility>>,
}

impl Tips {
    /// Must run on the main thread (e.g. from `setup`).
    pub fn new(app: &AppHandle) -> Self {
        if let Some(window) = app.get_webview_window(LABEL) {
            let _ = window.set_ignore_cursor_events(true);
            #[cfg(target_os = "macos")]
            overlay::configure_native(&window);
        }
        Self {
            app: app.clone(),
            visibility: Arc::default(),
        }
    }

    /// Shows `card` for `duration` next to any cards already on screen.
    pub fn show(&self, card: &Card, duration: Duration) {
        let _ = self.app.emit_to(
            LABEL,
            "tip",
            TipShown {
                card,
                duration_ms: duration.as_millis() as u64,
            },
        );

        let now = Instant::now();
        let (token, until, appearing) = {
            let mut visibility = self.visibility.lock().unwrap();
            let current = visibility.until.filter(|until| *until > now);
            let until = current.map_or(now + duration + FADE, |until| {
                until.max(now + duration + FADE)
            });
            visibility.generation += 1;
            visibility.until = Some(until);
            (visibility.generation, until, current.is_none())
        };

        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || {
            let Some(window) = app.get_webview_window(LABEL) else {
                return;
            };
            // Only place a fresh stack, so visible cards never jump between screens.
            if appearing {
                place_top_right(&app, &window);
            }
            overlay::order_front(&window);
        });

        let visibility = Arc::clone(&self.visibility);
        let app = self.app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            if visibility.lock().unwrap().generation != token {
                return;
            }
            let _ = app.clone().run_on_main_thread(move || {
                let mut visibility = visibility.lock().unwrap();
                if visibility.generation != token {
                    return;
                }
                visibility.until = None;
                if let Some(window) = app.get_webview_window(LABEL) {
                    overlay::order_out(&window);
                }
            });
        });
    }
}

fn place_top_right(app: &AppHandle, window: &WebviewWindow) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|cursor| app.monitor_from_point(cursor.x, cursor.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };

    let scale = monitor.scale_factor();
    let width = (WIDTH * scale).round() as u32;
    let height = (HEIGHT * scale).round() as u32;
    let margin = (MARGIN * scale).round() as i32;
    let area = monitor.work_area();
    let x = area.position.x + area.size.width as i32 - width as i32 - margin;
    let y = area.position.y + margin;

    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(width, height));
}
