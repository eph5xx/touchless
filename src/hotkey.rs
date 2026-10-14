//! Cmd+Option "tap" detection. Tauri's global-shortcut plugin needs a non-modifier
//! key, so a modifier-only chord is detected with a listen-only CGEventTap.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyStatus {
    Ready,
    NeedsPermission,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Modifiers {
    pub command: bool,
    pub option: bool,
    pub other: bool,
}

/// Fires once Cmd+Option were held together (and nothing else) and then all
/// modifiers are released. Any key press or extra modifier in between cancels it,
/// so shortcuts like Cmd+Option+Esc keep working normally.
#[derive(Debug, Default)]
pub struct ChordDetector {
    armed: bool,
    clean: bool,
}

impl ChordDetector {
    pub fn flags_changed(&mut self, modifiers: Modifiers) -> bool {
        if modifiers.command && modifiers.option && !modifiers.other && !self.armed {
            self.armed = true;
            self.clean = true;
            return false;
        }

        if !self.armed {
            return false;
        }

        if modifiers.other {
            self.clean = false;
        }

        if !modifiers.command && !modifiers.option && !modifiers.other {
            let fire = self.clean;
            self.armed = false;
            self.clean = false;
            return fire;
        }

        false
    }

    pub fn key_down(&mut self) {
        self.clean = false;
    }
}

#[cfg(target_os = "macos")]
pub fn spawn(on_toggle: impl Fn() + Send + 'static) -> HotkeyStatus {
    use std::cell::RefCell;

    use core_foundation::runloop::CFRunLoop;
    use core_graphics::event::{
        CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult,
    };

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }

    if !unsafe { CGPreflightListenEventAccess() } {
        unsafe { CGRequestListenEventAccess() };
        return HotkeyStatus::NeedsPermission;
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let detector = RefCell::new(ChordDetector::default());
            let other_flags = CGEventFlags::CGEventFlagShift
                | CGEventFlags::CGEventFlagControl
                | CGEventFlags::CGEventFlagAlphaShift;

            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
                |_proxy, event_type, event| {
                    match event_type {
                        CGEventType::KeyDown => detector.borrow_mut().key_down(),
                        CGEventType::FlagsChanged => {
                            let flags = event.get_flags();
                            let modifiers = Modifiers {
                                command: flags.contains(CGEventFlags::CGEventFlagCommand),
                                option: flags.contains(CGEventFlags::CGEventFlagAlternate),
                                other: flags.intersects(other_flags),
                            };
                            if detector.borrow_mut().flags_changed(modifiers) {
                                on_toggle();
                            }
                        }
                        _ => {}
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
pub fn spawn(_on_toggle: impl Fn() + Send + 'static) -> HotkeyStatus {
    HotkeyStatus::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        command: false,
        option: false,
        other: false,
    };
    const CMD: Modifiers = Modifiers {
        command: true,
        ..NONE
    };
    const OPT: Modifiers = Modifiers {
        option: true,
        ..NONE
    };
    const BOTH: Modifiers = Modifiers {
        command: true,
        option: true,
        other: false,
    };
    const BOTH_SHIFT: Modifiers = Modifiers {
        command: true,
        option: true,
        other: true,
    };

    fn run(detector: &mut ChordDetector, steps: &[Modifiers]) -> usize {
        steps.iter().filter(|&&m| detector.flags_changed(m)).count()
    }

    #[test]
    fn clean_tap_fires() {
        let mut d = ChordDetector::default();
        assert_eq!(run(&mut d, &[CMD, BOTH, OPT, NONE]), 1);
    }

    #[test]
    fn release_order_does_not_matter() {
        let mut d = ChordDetector::default();
        assert_eq!(run(&mut d, &[OPT, BOTH, CMD, NONE]), 1);
    }

    #[test]
    fn key_press_cancels() {
        let mut d = ChordDetector::default();
        run(&mut d, &[CMD, BOTH]);
        d.key_down();
        assert_eq!(run(&mut d, &[CMD, NONE]), 0);
    }

    #[test]
    fn extra_modifier_cancels() {
        let mut d = ChordDetector::default();
        assert_eq!(run(&mut d, &[CMD, BOTH, BOTH_SHIFT, BOTH, NONE]), 0);
    }

    #[test]
    fn single_modifier_does_not_fire() {
        let mut d = ChordDetector::default();
        assert_eq!(run(&mut d, &[CMD, NONE, OPT, NONE]), 0);
    }

    #[test]
    fn fires_again_after_reset() {
        let mut d = ChordDetector::default();
        assert_eq!(run(&mut d, &[BOTH, NONE, BOTH, NONE]), 2);
    }
}
