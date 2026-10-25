//! The cards at the top-right of the screen where plugin tips appear. Like the
//! pill the window never activates Touchless on its own. It is sized to the
//! cards, so clicks next to them still reach the app behind; a click on a card
//! opens that transcript in History.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize};

use crate::{overlay, plugin_api::Card};

const LABEL: &str = "tips";
const MARGIN: f64 = 12.0;
/// How often the pointer is checked while cards are on screen. Hover events
/// are not reliable in a window of an inactive app.
const HOVER_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Serialize)]
struct TipShown<'a> {
    card: &'a Card,
    duration_ms: u64,
    /// `None` for cards that belong to no dictation, such as examples.
    transcript_id: Option<i64>,
    /// A caption above the card.
    hint: Option<&'a str>,
}

/// The window frame in physical pixels.
#[derive(Clone, Copy)]
struct Frame {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl Frame {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.x as f64 + self.width as f64
            && y < self.y as f64 + self.height as f64
    }
}

/// Where the stack is pinned while it stays on screen.
#[derive(Clone, Copy)]
struct Anchor {
    right: i32,
    top: i32,
    scale: f64,
}

#[derive(Default)]
struct Placement {
    /// `None` while hidden.
    anchor: Option<Anchor>,
    frame: Option<Frame>,
    /// Bumped whenever the window appears or hides, which ends the previous hover poll.
    generation: u64,
}

pub struct Tips {
    app: AppHandle,
    placement: Arc<Mutex<Placement>>,
}

impl Tips {
    /// Must run on the main thread (e.g. from `setup`).
    pub fn new(app: &AppHandle) -> Self {
        #[cfg(target_os = "macos")]
        if let Some(window) = app.get_webview_window(LABEL) {
            overlay::configure_native(&window);
        }
        Self {
            app: app.clone(),
            placement: Arc::default(),
        }
    }

    /// Adds `card` to the stack. The page decides how long it stays and calls
    /// [`Self::fit`] once the stack has changed.
    pub fn show(&self, card: &Card, duration: Duration, transcript_id: i64) {
        self.emit(TipShown {
            card,
            duration_ms: duration.as_millis() as u64,
            transcript_id: Some(transcript_id),
            hint: None,
        });
    }

    /// Adds a sample card with `hint` above it; clicking it only dismisses it.
    pub fn show_example(&self, card: &Card, duration: Duration, hint: &str) {
        self.emit(TipShown {
            card,
            duration_ms: duration.as_millis() as u64,
            transcript_id: None,
            hint: Some(hint),
        });
    }

    fn emit(&self, tip: TipShown<'_>) {
        let _ = self.app.emit_to(LABEL, "tip", tip);
    }

    /// Sizes the window to a stack of `width` × `height` logical pixels, keeping
    /// its top-right corner in place, or hides it when the size is zero.
    pub fn fit(&self, width: f64, height: f64) {
        let app = self.app.clone();
        let placement = Arc::clone(&self.placement);
        let _ = self.app.run_on_main_thread(move || {
            let Some(window) = app.get_webview_window(LABEL) else {
                return;
            };
            let mut current = placement.lock().unwrap();
            if width <= 0.0 || height <= 0.0 {
                if current.anchor.take().is_some() {
                    current.frame = None;
                    current.generation += 1;
                }
                overlay::order_out(&window);
                return;
            }

            // Only place a fresh stack, so visible cards never jump between screens.
            let appearing = current.anchor.is_none();
            let anchor = match current.anchor {
                Some(anchor) => anchor,
                None => match top_right(&app) {
                    Some(anchor) => anchor,
                    None => return,
                },
            };
            let width = (width * anchor.scale).ceil() as u32;
            let frame = Frame {
                x: anchor.right - width as i32,
                y: anchor.top,
                width,
                height: (height * anchor.scale).ceil() as u32,
            };
            current.anchor = Some(anchor);
            current.frame = Some(frame);
            let _ = window.set_size(PhysicalSize::new(frame.width, frame.height));
            let _ = window.set_position(PhysicalPosition::new(frame.x, frame.y));
            overlay::order_front(&window);

            if appearing {
                current.generation += 1;
                watch_hover(app.clone(), Arc::clone(&placement), current.generation);
            }
        });
    }
}

/// Tells the page whether the pointer is over the cards until the window hides.
fn watch_hover(app: AppHandle, placement: Arc<Mutex<Placement>>, generation: u64) {
    std::thread::spawn(move || {
        let mut hovering = false;
        loop {
            std::thread::sleep(HOVER_POLL);
            let frame = {
                let placement = placement.lock().unwrap();
                if placement.generation != generation {
                    return;
                }
                placement.frame
            };
            let inside = match (frame, app.cursor_position()) {
                (Some(frame), Ok(cursor)) => frame.contains(cursor.x, cursor.y),
                _ => false,
            };
            if inside != hovering {
                hovering = inside;
                let _ = app.emit_to(LABEL, "tips-hover", hovering);
            }
        }
    });
}

fn top_right(app: &AppHandle) -> Option<Anchor> {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|cursor| app.monitor_from_point(cursor.x, cursor.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let margin = (MARGIN * scale).round() as i32;
    let area = monitor.work_area();
    Some(Anchor {
        right: area.position.x + area.size.width as i32 - margin,
        top: area.position.y + margin,
        scale,
    })
}
