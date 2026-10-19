//! The floating "pill" that shows dictation progress over other apps. It must never
//! become key or activate Touchless, otherwise the paste would land in Touchless.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

const LABEL: &str = "overlay";
const WIDTH: f64 = 520.0;
/// Tall enough for a plugin card above the pill. The window is transparent and
/// ignores the mouse, so the empty part never gets in the way.
const HEIGHT: f64 = 340.0;
const BOTTOM_MARGIN: f64 = 24.0;

pub struct Overlay {
    app: AppHandle,
    /// Bumped on every show so that a pending delayed hide from the previous
    /// dictation does not hide the pill of the current one.
    generation: Arc<AtomicU64>,
}

impl Overlay {
    /// Must run on the main thread (e.g. from `setup`).
    pub fn new(app: &AppHandle) -> Self {
        if let Some(window) = app.get_webview_window(LABEL) {
            let _ = window.set_ignore_cursor_events(true);
            #[cfg(target_os = "macos")]
            configure_native(&window);
        }
        Self {
            app: app.clone(),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns a token for [`Self::hide_after`].
    pub fn show(&self) -> u64 {
        let token = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || {
            let Some(window) = app.get_webview_window(LABEL) else {
                return;
            };
            place_near_cursor(&app, &window);
            order_front(&window);
        });
        token
    }

    /// Hides now and cancels pending hides.
    pub fn hide(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || {
            if let Some(window) = app.get_webview_window(LABEL) {
                order_out(&window);
            }
        });
    }

    /// Hides after `delay`, unless the overlay was shown or hidden again since the
    /// `show` that returned `token`.
    pub fn hide_after(&self, token: u64, delay: Duration) {
        let generation = Arc::clone(&self.generation);
        let expected = token;
        let app = self.app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if generation.load(Ordering::SeqCst) != expected {
                return;
            }
            let _ = app.clone().run_on_main_thread(move || {
                if generation.load(Ordering::SeqCst) == expected
                    && let Some(window) = app.get_webview_window(LABEL)
                {
                    order_out(&window);
                }
            });
        });
    }
}

fn place_near_cursor(app: &AppHandle, window: &WebviewWindow) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|cursor| app.monitor_from_point(cursor.x, cursor.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };

    let scale = monitor.scale_factor();
    let width = (WIDTH * scale).round() as u32;
    let height = (HEIGHT * scale).round() as u32;
    let area = monitor.work_area();
    let x = area.position.x + (area.size.width as i32 - width as i32) / 2;
    let y =
        area.position.y + area.size.height as i32 - height as i32 - (BOTTOM_MARGIN * scale) as i32;

    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(width, height));
}

#[cfg(target_os = "macos")]
fn ns_window(window: &WebviewWindow) -> Option<&objc2_app_kit::NSWindow> {
    let pointer = window.ns_window().ok()?;
    // SAFETY: Tauri returns the live NSWindow backing this window; callers are on the
    // main thread and do not outlive `window`.
    unsafe { pointer.cast::<objc2_app_kit::NSWindow>().as_ref() }
}

#[cfg(target_os = "macos")]
fn configure_native(window: &WebviewWindow) {
    use objc2_app_kit::{NSStatusWindowLevel, NSWindowCollectionBehavior};

    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.setLevel(NSStatusWindowLevel);
    ns_window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns_window.setHidesOnDeactivate(false);
}

#[cfg(target_os = "macos")]
fn order_front(window: &WebviewWindow) {
    // Unlike `show()`, this does not make the window key or activate the app.
    if let Some(ns_window) = ns_window(window) {
        ns_window.orderFrontRegardless();
    }
}

#[cfg(target_os = "macos")]
fn order_out(window: &WebviewWindow) {
    if let Some(ns_window) = ns_window(window) {
        ns_window.orderOut(None);
    }
}

#[cfg(not(target_os = "macos"))]
fn order_front(window: &WebviewWindow) {
    let _ = window.show();
}

#[cfg(not(target_os = "macos"))]
fn order_out(window: &WebviewWindow) {
    let _ = window.hide();
}
