//! The dictation shortcut: modifier keys tapped on their own (⌥⌘, Right ⌥, fn)
//! or a regular key pressed with modifiers (⌥Space).

use serde::{Deserialize, Serialize};

/// Modifier keys, in macOS display order (fn ⌃ ⌥ ⇧ ⌘). Side-specific keys only
/// appear in a tap of that single key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Fn,
    Control,
    Option,
    Shift,
    Command,
    LeftControl,
    RightControl,
    LeftOption,
    RightOption,
    LeftShift,
    RightShift,
    LeftCommand,
    RightCommand,
}

const ALL_KEYS: [Key; 13] = [
    Key::Fn,
    Key::Control,
    Key::Option,
    Key::Shift,
    Key::Command,
    Key::LeftControl,
    Key::RightControl,
    Key::LeftOption,
    Key::RightOption,
    Key::LeftShift,
    Key::RightShift,
    Key::LeftCommand,
    Key::RightCommand,
];

impl Key {
    fn without_side(self) -> Key {
        match self {
            Key::LeftControl | Key::RightControl => Key::Control,
            Key::LeftOption | Key::RightOption => Key::Option,
            Key::LeftShift | Key::RightShift => Key::Shift,
            Key::LeftCommand | Key::RightCommand => Key::Command,
            key => key,
        }
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shortcut {
    /// Modifier keys pressed and released with nothing else pressed in between.
    Tap { keys: Vec<Key> },
    /// A regular key pressed while holding modifiers; `key` is a W3C
    /// `KeyboardEvent.code` name such as `Space` or `KeyD`.
    Combo { modifiers: Vec<Key>, key: String },
}

impl Default for Shortcut {
    fn default() -> Self {
        Self::Tap {
            keys: vec![Key::Option, Key::Command],
        }
    }
}

impl Shortcut {
    /// The shortcut made by tapping `held`.
    pub fn tap(held: Held) -> Self {
        Self::Tap {
            keys: tap_keys(held.keys()),
        }
    }

    /// The shortcut made by pressing the key with this macOS virtual keycode.
    pub fn combo(held: Held, keycode: u16) -> Result<Self, String> {
        let key = KEY_CODES
            .iter()
            .find(|(code, _)| *code == keycode)
            .map(|(_, name)| *name)
            .ok_or("This key can't be used in a shortcut")?;
        // macOS also reports fn for arrows and function keys, so it is ignored here.
        let modifiers = held
            .keys()
            .into_iter()
            .map(Key::without_side)
            .filter(|key| *key != Key::Fn)
            .collect();
        Self::Combo {
            modifiers,
            key: key.to_owned(),
        }
        .normalized()
    }

    pub fn matches_tap(&self, held: Held) -> bool {
        matches!(self, Self::Tap { keys } if *keys == tap_keys(held.keys()))
    }

    pub fn normalized(self) -> Result<Self, String> {
        match self {
            Self::Tap { keys } if keys.is_empty() => Err("Tap at least one modifier key".into()),
            Self::Tap { keys } => Ok(Self::Tap {
                keys: tap_keys(keys),
            }),
            Self::Combo { modifiers, key } => {
                if !KEY_CODES.iter().any(|(_, name)| *name == key) {
                    return Err(format!("\"{key}\" can't be used in a shortcut"));
                }
                if modifiers.contains(&Key::Fn) {
                    return Err("fn only works in a tap of modifier keys".into());
                }
                let mut modifiers: Vec<Key> =
                    modifiers.into_iter().map(Key::without_side).collect();
                modifiers.sort();
                modifiers.dedup();
                let blocks_typing = !modifiers
                    .iter()
                    .any(|key| matches!(key, Key::Control | Key::Option | Key::Command));
                if blocks_typing && !is_function_key(&key) {
                    return Err(
                        "Add ⌘, ⌥ or ⌃ so the shortcut doesn't get in the way of typing".into(),
                    );
                }
                Ok(Self::Combo { modifiers, key })
            }
        }
    }
}

/// A single key keeps its side so Right ⌥ doesn't fire on Left ⌥; with several
/// keys the side doesn't matter.
fn tap_keys(mut keys: Vec<Key>) -> Vec<Key> {
    keys.sort();
    keys.dedup();
    if keys.len() > 1 {
        keys = keys.into_iter().map(Key::without_side).collect();
        keys.sort();
        keys.dedup();
    }
    keys
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=20).contains(&number))
}

/// Physical modifier keys held down, one bit per `Key`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held(u16);

// CGEventFlags: the generic modifier masks plus the device-dependent
// NX_DEVICE*KEYMASK bits that tell left from right.
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_FN: u64 = 0x0080_0000;
#[rustfmt::skip]
const SIDES: [(u64, u64, u64, Key, Key); 4] = [
    (FLAG_CONTROL, 0x0001, 0x2000, Key::LeftControl, Key::RightControl),
    (FLAG_OPTION, 0x0020, 0x0040, Key::LeftOption, Key::RightOption),
    (FLAG_SHIFT, 0x0002, 0x0004, Key::LeftShift, Key::RightShift),
    (FLAG_COMMAND, 0x0008, 0x0010, Key::LeftCommand, Key::RightCommand),
];

impl Held {
    pub const NONE: Self = Self(0);

    /// Reads `CGEventFlags` bits. Caps Lock is a toggle, not a held key, so it is ignored.
    pub fn from_flags(flags: u64) -> Self {
        let mut held = Self::default();
        if flags & FLAG_FN != 0 {
            held.0 |= Key::Fn.bit();
        }
        for (generic, left_bit, right_bit, left, right) in SIDES {
            if flags & generic == 0 {
                continue;
            }
            let is_right = flags & right_bit != 0;
            // Synthetic or remapped events may carry only the generic flag.
            if flags & left_bit != 0 || !is_right {
                held.0 |= left.bit();
            }
            if is_right {
                held.0 |= right.bit();
            }
        }
        held
    }

    pub fn is_empty(self) -> bool {
        self == Self::NONE
    }

    pub fn union(self, other: Held) -> Held {
        Held(self.0 | other.0)
    }

    fn keys(self) -> Vec<Key> {
        ALL_KEYS
            .into_iter()
            .filter(|key| self.0 & key.bit() != 0)
            .collect()
    }

    #[cfg(test)]
    pub fn of(keys: &[Key]) -> Self {
        Self(keys.iter().fold(0, |bits, key| bits | key.bit()))
    }
}

/// macOS virtual keycodes (US layout positions) of the keys a combo can use.
const KEY_CODES: &[(u16, &str)] = &[
    (0x00, "KeyA"),
    (0x01, "KeyS"),
    (0x02, "KeyD"),
    (0x03, "KeyF"),
    (0x04, "KeyH"),
    (0x05, "KeyG"),
    (0x06, "KeyZ"),
    (0x07, "KeyX"),
    (0x08, "KeyC"),
    (0x09, "KeyV"),
    (0x0B, "KeyB"),
    (0x0C, "KeyQ"),
    (0x0D, "KeyW"),
    (0x0E, "KeyE"),
    (0x0F, "KeyR"),
    (0x10, "KeyY"),
    (0x11, "KeyT"),
    (0x12, "Digit1"),
    (0x13, "Digit2"),
    (0x14, "Digit3"),
    (0x15, "Digit4"),
    (0x16, "Digit6"),
    (0x17, "Digit5"),
    (0x18, "Equal"),
    (0x19, "Digit9"),
    (0x1A, "Digit7"),
    (0x1B, "Minus"),
    (0x1C, "Digit8"),
    (0x1D, "Digit0"),
    (0x1E, "BracketRight"),
    (0x1F, "KeyO"),
    (0x20, "KeyU"),
    (0x21, "BracketLeft"),
    (0x22, "KeyI"),
    (0x23, "KeyP"),
    (0x24, "Enter"),
    (0x25, "KeyL"),
    (0x26, "KeyJ"),
    (0x27, "Quote"),
    (0x28, "KeyK"),
    (0x29, "Semicolon"),
    (0x2A, "Backslash"),
    (0x2B, "Comma"),
    (0x2C, "Slash"),
    (0x2D, "KeyN"),
    (0x2E, "KeyM"),
    (0x2F, "Period"),
    (0x30, "Tab"),
    (0x31, "Space"),
    (0x32, "Backquote"),
    (0x33, "Backspace"),
    (0x40, "F17"),
    (0x4F, "F18"),
    (0x50, "F19"),
    (0x5A, "F20"),
    (0x60, "F5"),
    (0x61, "F6"),
    (0x62, "F7"),
    (0x63, "F3"),
    (0x64, "F8"),
    (0x65, "F9"),
    (0x67, "F11"),
    (0x69, "F13"),
    (0x6A, "F16"),
    (0x6B, "F14"),
    (0x6D, "F10"),
    (0x6F, "F12"),
    (0x71, "F15"),
    (0x73, "Home"),
    (0x74, "PageUp"),
    (0x75, "Delete"),
    (0x76, "F4"),
    (0x77, "End"),
    (0x78, "F2"),
    (0x79, "PageDown"),
    (0x7A, "F1"),
    (0x7B, "ArrowLeft"),
    (0x7C, "ArrowRight"),
    (0x7D, "ArrowDown"),
    (0x7E, "ArrowUp"),
];

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: u16 = 0x31;
    const D: u16 = 0x02;
    const F13: u16 = 0x69;

    fn tap(keys: &[Key]) -> Shortcut {
        Shortcut::Tap {
            keys: keys.to_vec(),
        }
    }

    #[test]
    fn default_is_option_command_tap() {
        assert_eq!(Shortcut::default(), tap(&[Key::Option, Key::Command]));
        let json = serde_json::to_string(&Shortcut::default()).unwrap();
        assert_eq!(json, r#"{"kind":"tap","keys":["option","command"]}"#);
        assert_eq!(
            serde_json::from_str::<Shortcut>(&json).unwrap(),
            Shortcut::default()
        );
    }

    #[test]
    fn reads_sides_from_event_flags() {
        let left_command = FLAG_COMMAND | 0x0008;
        let right_option = FLAG_OPTION | 0x0040;
        assert_eq!(
            Held::from_flags(left_command | right_option),
            Held::of(&[Key::LeftCommand, Key::RightOption])
        );
        assert_eq!(
            Held::from_flags(FLAG_CONTROL | 0x2000 | FLAG_FN),
            Held::of(&[Key::RightControl, Key::Fn])
        );
    }

    #[test]
    fn generic_flag_without_side_counts_as_left() {
        assert_eq!(Held::from_flags(FLAG_SHIFT), Held::of(&[Key::LeftShift]));
    }

    #[test]
    fn ignores_caps_lock() {
        assert!(Held::from_flags(0x0001_0000).is_empty());
    }

    #[test]
    fn single_key_tap_keeps_its_side() {
        assert_eq!(
            Shortcut::tap(Held::of(&[Key::RightOption])),
            tap(&[Key::RightOption])
        );
        assert_eq!(Shortcut::tap(Held::of(&[Key::Fn])), tap(&[Key::Fn]));
    }

    #[test]
    fn multi_key_tap_ignores_sides() {
        let shortcut = Shortcut::tap(Held::of(&[Key::RightCommand, Key::LeftOption]));
        assert_eq!(shortcut, tap(&[Key::Option, Key::Command]));
        assert!(shortcut.matches_tap(Held::of(&[Key::LeftCommand, Key::RightOption])));
        assert!(!shortcut.matches_tap(Held::of(&[Key::LeftCommand])));
        assert!(!shortcut.matches_tap(Held::of(&[
            Key::LeftCommand,
            Key::LeftOption,
            Key::LeftShift
        ])));
    }

    #[test]
    fn single_key_matches_only_its_side() {
        let shortcut = tap(&[Key::RightOption]);
        assert!(shortcut.matches_tap(Held::of(&[Key::RightOption])));
        assert!(!shortcut.matches_tap(Held::of(&[Key::LeftOption])));
    }

    #[test]
    fn combo_from_keycode() {
        let held = Held::of(&[Key::RightOption, Key::Fn]);
        assert_eq!(
            Shortcut::combo(held, SPACE).unwrap(),
            Shortcut::Combo {
                modifiers: vec![Key::Option],
                key: "Space".into()
            }
        );
        assert!(Shortcut::combo(held, 0x35).is_err());
    }

    #[test]
    fn combo_needs_a_modifier_unless_it_is_a_function_key() {
        assert!(Shortcut::combo(Held::NONE, D).is_err());
        assert!(Shortcut::combo(Held::of(&[Key::LeftShift]), D).is_err());
        assert!(Shortcut::combo(Held::of(&[Key::LeftShift, Key::LeftCommand]), D).is_ok());
        assert!(Shortcut::combo(Held::NONE, F13).is_ok());
    }

    #[test]
    fn normalizes_stored_shortcuts() {
        assert_eq!(
            tap(&[Key::Command, Key::LeftOption, Key::Command])
                .normalized()
                .unwrap(),
            tap(&[Key::Option, Key::Command])
        );
        assert!(tap(&[]).normalized().is_err());

        let combo = Shortcut::Combo {
            modifiers: vec![Key::Command, Key::LeftShift],
            key: "KeyD".into(),
        };
        assert_eq!(
            combo.normalized().unwrap(),
            Shortcut::Combo {
                modifiers: vec![Key::Shift, Key::Command],
                key: "KeyD".into()
            }
        );

        let unknown = Shortcut::Combo {
            modifiers: vec![Key::Command],
            key: "Launch".into(),
        };
        assert!(unknown.normalized().is_err());

        let with_fn = Shortcut::Combo {
            modifiers: vec![Key::Fn, Key::Command],
            key: "KeyD".into(),
        };
        assert!(with_fn.normalized().is_err());
    }

    #[test]
    fn key_codes_are_unique() {
        for (index, (code, name)) in KEY_CODES.iter().enumerate() {
            assert!(
                KEY_CODES[index + 1..]
                    .iter()
                    .all(|(other_code, other_name)| other_code != code && other_name != name),
                "{name} is listed twice"
            );
        }
    }
}
