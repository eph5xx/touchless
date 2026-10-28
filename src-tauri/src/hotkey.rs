//! The global dictation shortcut. Carbon hotkeys (the global-shortcut plugin)
//! need a regular key, so modifier-only taps are detected with a listen-only
//! CGEventTap, which also records new shortcuts. Combos with a regular key go
//! through the plugin, which also keeps the keystroke from reaching the active app.
//! While dictating, Esc is registered the same way so it cancels without
//! reaching the active app.

use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut as HotKey, ShortcutState,
};
use tokio::sync::oneshot;

use crate::shortcut::{Held, Key, Shortcut};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyStatus {
    Ready,
    NeedsPermission,
    Unavailable,
}

/// Reports modifier keys that were pressed and all released again. A regular
/// key or a click in between cancels the tap, so shortcuts like ⌥⌘Esc and
/// ⌘-click keep working normally.
#[derive(Debug, Default)]
pub struct TapDetector {
    seen: Held,
    clean: bool,
}

impl TapDetector {
    /// Returns every key that was part of the tap once the last one is released.
    pub fn flags_changed(&mut self, held: Held) -> Option<Held> {
        if held.is_empty() {
            let tapped = (self.clean && !self.seen.is_empty()).then_some(self.seen);
            *self = Self::default();
            return tapped;
        }
        if self.seen.is_empty() {
            self.clean = true;
        }
        self.seen = self.seen.union(held);
        None
    }

    pub fn interrupt(&mut self) {
        self.clean = false;
    }
}

type Toggle = Arc<dyn Fn() + Send + Sync>;
/// `None` means recording was cancelled.
type Captured = Option<Result<Shortcut, String>>;

const ESCAPE: u16 = 0x35;

/// State shared with the event tap thread.
struct Shared {
    shortcut: Shortcut,
    /// Set while recording a new shortcut; the next tap or key goes here.
    capture: Option<oneshot::Sender<Captured>>,
}

impl Shared {
    /// Returns whether the tap should toggle dictation.
    fn tapped(&mut self, held: Held) -> bool {
        match self.capture.take() {
            Some(capture) => {
                let _ = capture.send(Some(Ok(Shortcut::tap(held))));
                false
            }
            None => self.shortcut.matches_tap(held),
        }
    }

    fn key_down(&mut self, keycode: u16, held: Held) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        let captured = if keycode == ESCAPE && held.is_empty() {
            None
        } else {
            Some(Shortcut::combo(held, keycode))
        };
        let _ = capture.send(captured);
    }

    fn cancel_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            let _ = capture.send(None);
        }
    }
}

pub struct Hotkey {
    app: AppHandle,
    shared: Arc<Mutex<Shared>>,
    toggle: Toggle,
    tap: HotkeyStatus,
    /// The combo currently registered with the global-shortcut plugin.
    combo: Mutex<Option<HotKey>>,
    escape: Mutex<std_mpsc::Sender<bool>>,
}

impl Hotkey {
    pub fn start(
        app: &AppHandle,
        shortcut: Shortcut,
        toggle: impl Fn() + Send + Sync + 'static,
        cancel: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let toggle: Toggle = Arc::new(toggle);
        let escape = spawn_escape(app.clone(), Arc::new(cancel));
        let shared = Arc::new(Mutex::new(Shared {
            shortcut: shortcut.clone(),
            capture: None,
        }));
        let tap = spawn_tap(Arc::clone(&shared), Arc::clone(&toggle));
        let hotkey = Self {
            app: app.clone(),
            shared,
            toggle,
            tap,
            combo: Mutex::new(None),
            escape: Mutex::new(escape),
        };
        if let Err(error) = hotkey.apply(&shortcut) {
            eprintln!("could not start the shortcut: {error}");
        }
        hotkey
    }

    pub fn status(&self) -> HotkeyStatus {
        let is_tap = matches!(self.shared.lock().unwrap().shortcut, Shortcut::Tap { .. });
        if is_tap {
            self.tap
        } else if self.combo.lock().unwrap().is_some() {
            HotkeyStatus::Ready
        } else {
            HotkeyStatus::Unavailable
        }
    }

    pub fn apply(&self, shortcut: &Shortcut) -> Result<(), String> {
        self.register(combo_hotkey(shortcut)?)?;
        self.shared.lock().unwrap().shortcut = shortcut.clone();
        Ok(())
    }

    /// Waits for the user to press a new shortcut; `None` if they cancelled.
    /// The caller re-applies a shortcut afterwards, since a combo is paused here.
    pub async fn capture(&self) -> Result<Option<Shortcut>, String> {
        if self.tap != HotkeyStatus::Ready {
            return Err(
                "Allow Input Monitoring to record a shortcut, then restart Touchless".into(),
            );
        }
        // Otherwise pressing the current combo would start dictation.
        self.register(None)?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut shared = self.shared.lock().unwrap();
            shared.cancel_capture();
            shared.capture = Some(sender);
        }
        receiver.await.ok().flatten().transpose()
    }

    pub fn cancel_capture(&self) {
        self.shared.lock().unwrap().cancel_capture();
    }

    /// Takes over Esc while dictating so it cancels. Never blocks: the plugin
    /// waits for the main thread, which may be the caller.
    pub fn set_dictating(&self, dictating: bool) {
        let _ = self.escape.lock().unwrap().send(dictating);
    }

    fn register(&self, next: Option<HotKey>) -> Result<(), String> {
        let mut current = self.combo.lock().unwrap();
        if *current == next {
            return Ok(());
        }
        let shortcuts = self.app.global_shortcut();
        if let Some(previous) = *current {
            shortcuts
                .unregister(previous)
                .map_err(|error| format!("could not release the previous shortcut: {error}"))?;
            *current = None;
        }
        if let Some(hotkey) = next {
            let toggle = Arc::clone(&self.toggle);
            shortcuts
                .on_shortcut(hotkey, move |_app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle();
                    }
                })
                .map_err(|error| {
                    eprintln!("could not register {hotkey:?}: {error}");
                    "Couldn't use this shortcut. Another app may already use it.".to_owned()
                })?;
            *current = Some(hotkey);
        }
        Ok(())
    }
}

/// Registers or releases Esc on its own thread, always applying the latest wanted state.
fn spawn_escape(app: AppHandle, cancel: Toggle) -> std_mpsc::Sender<bool> {
    let (sender, receiver) = std_mpsc::channel::<bool>();
    let spawned = std::thread::Builder::new()
        .name("escape".into())
        .spawn(move || {
            let escape = HotKey::new(None, Code::Escape);
            let shortcuts = app.global_shortcut();
            let mut registered = false;
            while let Ok(mut wanted) = receiver.recv() {
                while let Ok(next) = receiver.try_recv() {
                    wanted = next;
                }
                if wanted == registered {
                    continue;
                }
                let result = if wanted {
                    let cancel = Arc::clone(&cancel);
                    shortcuts.on_shortcut(escape, move |_app, _shortcut, event| {
                        if event.state == ShortcutState::Pressed {
                            cancel();
                        }
                    })
                } else {
                    shortcuts.unregister(escape)
                };
                match result {
                    Ok(()) => registered = wanted,
                    Err(error) => eprintln!("could not update the Esc shortcut: {error}"),
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("could not start the Esc shortcut: {error}");
    }
    sender
}

fn combo_hotkey(shortcut: &Shortcut) -> Result<Option<HotKey>, String> {
    let Shortcut::Combo { modifiers, key } = shortcut else {
        return Ok(None);
    };
    let code: Code = key
        .parse()
        .map_err(|_| format!("\"{key}\" can't be used in a shortcut"))?;
    let modifiers = modifiers
        .iter()
        .fold(Modifiers::empty(), |all, modifier| match modifier {
            Key::Control => all | Modifiers::CONTROL,
            Key::Option => all | Modifiers::ALT,
            Key::Shift => all | Modifiers::SHIFT,
            Key::Command => all | Modifiers::SUPER,
            _ => all,
        });
    Ok(Some(HotKey::new(Some(modifiers), code)))
}

#[cfg(target_os = "macos")]
fn spawn_tap(shared: Arc<Mutex<Shared>>, toggle: Toggle) -> HotkeyStatus {
    use std::cell::RefCell;

    use core_foundation::runloop::CFRunLoop;
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
    };

    if !crate::permissions::input_monitoring_granted() {
        crate::permissions::request_input_monitoring();
        return HotkeyStatus::NeedsPermission;
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let detector = RefCell::new(TapDetector::default());

            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![
                    CGEventType::FlagsChanged,
                    CGEventType::KeyDown,
                    CGEventType::LeftMouseDown,
                    CGEventType::RightMouseDown,
                    CGEventType::OtherMouseDown,
                ],
                |_proxy, event_type, event| {
                    let held = Held::from_flags(event.get_flags().bits());
                    match event_type {
                        CGEventType::FlagsChanged => {
                            let tapped = detector.borrow_mut().flags_changed(held);
                            if tapped.is_some_and(|keys| shared.lock().unwrap().tapped(keys)) {
                                toggle();
                            }
                        }
                        CGEventType::KeyDown => {
                            detector.borrow_mut().interrupt();
                            let repeat = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT);
                            if repeat == 0 {
                                let keycode = event
                                    .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                                shared.lock().unwrap().key_down(keycode as u16, held);
                            }
                        }
                        _ => detector.borrow_mut().interrupt(),
                    }
                    CallbackResult::Keep
                },
                || {
                    let _ = ready_tx.send(true);
                    CFRunLoop::run_current();
                },
            );

            if result.is_err() {
                let _ = ready_tx.send(false);
            }
        })
        .expect("failed to spawn hotkey thread");

    match ready_rx.recv() {
        Ok(true) => HotkeyStatus::Ready,
        _ => HotkeyStatus::Unavailable,
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_tap(_shared: Arc<Mutex<Shared>>, _toggle: Toggle) -> HotkeyStatus {
    HotkeyStatus::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Held = Held::NONE;

    fn held(keys: &[Key]) -> Held {
        Held::of(keys)
    }

    fn run(detector: &mut TapDetector, steps: &[Held]) -> Vec<Held> {
        steps
            .iter()
            .filter_map(|&step| detector.flags_changed(step))
            .collect()
    }

    #[test]
    fn reports_a_clean_tap() {
        let mut d = TapDetector::default();
        let cmd = held(&[Key::LeftCommand]);
        let both = held(&[Key::LeftCommand, Key::RightOption]);
        let opt = held(&[Key::RightOption]);
        assert_eq!(run(&mut d, &[cmd, both, opt, NONE]), [both]);
    }

    #[test]
    fn reports_every_key_pressed_during_the_tap() {
        let mut d = TapDetector::default();
        let cmd = held(&[Key::LeftCommand]);
        let opt = held(&[Key::LeftOption]);
        let both = cmd.union(opt);
        assert_eq!(run(&mut d, &[cmd, NONE]), [cmd]);
        assert_eq!(run(&mut d, &[cmd, both, opt, NONE]), [both]);
    }

    #[test]
    fn key_press_cancels() {
        let mut d = TapDetector::default();
        let both = held(&[Key::LeftCommand, Key::LeftOption]);
        run(&mut d, &[both]);
        d.interrupt();
        assert!(run(&mut d, &[NONE]).is_empty());
        assert_eq!(run(&mut d, &[both, NONE]), [both]);
    }

    #[test]
    fn interrupt_before_the_tap_is_ignored() {
        let mut d = TapDetector::default();
        d.interrupt();
        let fn_key = held(&[Key::Fn]);
        assert_eq!(run(&mut d, &[fn_key, NONE]), [fn_key]);
    }

    #[test]
    fn matches_the_configured_shortcut() {
        let mut shared = Shared {
            shortcut: Shortcut::default(),
            capture: None,
        };
        assert!(shared.tapped(held(&[Key::RightCommand, Key::LeftOption])));
        assert!(!shared.tapped(held(&[Key::LeftCommand])));
        assert!(!shared.tapped(held(&[Key::LeftCommand, Key::LeftOption, Key::LeftShift])));
    }

    #[test]
    fn capture_takes_the_next_tap_instead_of_toggling() {
        let (sender, mut receiver) = oneshot::channel();
        let mut shared = Shared {
            shortcut: Shortcut::default(),
            capture: Some(sender),
        };
        assert!(!shared.tapped(held(&[Key::LeftCommand, Key::LeftOption])));
        assert_eq!(
            receiver.try_recv().unwrap(),
            Some(Ok(Shortcut::Tap {
                keys: vec![Key::Option, Key::Command]
            }))
        );
        assert!(shared.tapped(held(&[Key::LeftCommand, Key::LeftOption])));
    }

    #[test]
    fn capture_takes_a_combo() {
        let (sender, mut receiver) = oneshot::channel();
        let mut shared = Shared {
            shortcut: Shortcut::default(),
            capture: Some(sender),
        };
        shared.key_down(0x31, held(&[Key::LeftOption]));
        assert_eq!(
            receiver.try_recv().unwrap(),
            Some(Ok(Shortcut::Combo {
                modifiers: vec![Key::Option],
                key: "Space".into()
            }))
        );
    }

    #[test]
    fn escape_cancels_capture() {
        let (sender, mut receiver) = oneshot::channel();
        let mut shared = Shared {
            shortcut: Shortcut::default(),
            capture: Some(sender),
        };
        shared.key_down(ESCAPE, NONE);
        assert_eq!(receiver.try_recv().unwrap(), None);
        assert!(shared.capture.is_none());
    }

    #[test]
    fn converts_combos_for_the_plugin() {
        let combo = Shortcut::Combo {
            modifiers: vec![Key::Shift, Key::Command],
            key: "KeyD".into(),
        };
        assert_eq!(
            combo_hotkey(&combo).unwrap(),
            Some(HotKey::new(
                Some(Modifiers::SHIFT | Modifiers::SUPER),
                Code::KeyD
            ))
        );
        assert_eq!(combo_hotkey(&Shortcut::default()).unwrap(), None);
    }
}
