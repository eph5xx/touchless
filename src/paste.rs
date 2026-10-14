//! Delivers a finished transcript to the app the user is typing in: the text is put
//! on the clipboard, ⌘V is synthesized, and the previous clipboard is restored.

use serde::Serialize;

use crate::settings::Preferences;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Pasted into the focused app.
    Pasted,
    /// Left on the clipboard (Touchless itself was focused, or paste is not permitted).
    Copied,
    /// Only saved to history.
    Saved,
}

pub fn deliver(
    text: &str,
    preferences: &Preferences,
    touchless_focused: bool,
) -> Result<Delivery, String> {
    if !preferences.auto_paste {
        return Ok(Delivery::Saved);
    }
    if touchless_focused || !crate::permissions::accessibility_granted() {
        platform::copy(text)?;
        return Ok(Delivery::Copied);
    }
    platform::paste(text, preferences.restore_clipboard)?;
    Ok(Delivery::Pasted)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{thread, time::Duration};

    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode},
        event_source::{CGEventSource, CGEventSourceStateID},
    };
    use objc2::{rc::Retained, runtime::ProtocolObject};
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSData, NSString};

    const V_KEY: CGKeyCode = 9;
    /// Time the target app gets to read the clipboard before it is restored.
    const RESTORE_DELAY: Duration = Duration::from_millis(500);

    type ClipboardItem = Vec<(String, Vec<u8>)>;

    pub fn copy(text: &str) -> Result<(), String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        write_text(&pasteboard, text)
    }

    pub fn paste(text: &str, restore: bool) -> Result<(), String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let previous = restore.then(|| snapshot(&pasteboard));
        write_text(&pasteboard, text)?;
        let written = pasteboard.changeCount();
        post_command_v()?;

        if let Some(previous) = previous {
            thread::spawn(move || {
                thread::sleep(RESTORE_DELAY);
                let pasteboard = NSPasteboard::generalPasteboard();
                // Leave the clipboard alone if anything else wrote to it meanwhile.
                if pasteboard.changeCount() == written {
                    restore_snapshot(&pasteboard, &previous);
                }
            });
        }
        Ok(())
    }

    fn write_text(pasteboard: &NSPasteboard, text: &str) -> Result<(), String> {
        pasteboard.clearContents();
        let written = pasteboard
            .setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
        if written {
            Ok(())
        } else {
            Err("could not write to the clipboard".into())
        }
    }

    fn snapshot(pasteboard: &NSPasteboard) -> Vec<ClipboardItem> {
        let Some(items) = pasteboard.pasteboardItems() else {
            return Vec::new();
        };
        items
            .iter()
            .map(|item| {
                item.types()
                    .iter()
                    .filter_map(|data_type| {
                        let data = item.dataForType(&data_type)?;
                        Some((data_type.to_string(), data.to_vec()))
                    })
                    .collect()
            })
            .collect()
    }

    fn restore_snapshot(pasteboard: &NSPasteboard, items: &[ClipboardItem]) {
        pasteboard.clearContents();
        if items.is_empty() {
            return;
        }
        let items: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = items
            .iter()
            .map(|entries| {
                let item = NSPasteboardItem::new();
                for (data_type, bytes) in entries {
                    item.setData_forType(
                        &NSData::with_bytes(bytes),
                        &NSString::from_str(data_type),
                    );
                }
                ProtocolObject::from_retained(item)
            })
            .collect();
        pasteboard.writeObjects(&NSArray::from_retained_slice(&items));
    }

    fn post_command_v() -> Result<(), String> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| "could not create a keyboard event source".to_owned())?;
        for key_down in [true, false] {
            let event = CGEvent::new_keyboard_event(source.clone(), V_KEY, key_down)
                .map_err(|()| "could not create a keyboard event".to_owned())?;
            event.set_flags(CGEventFlags::CGEventFlagCommand);
            event.post(CGEventTapLocation::HID);
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn copy(_text: &str) -> Result<(), String> {
        Err("clipboard access is only available on macOS".into())
    }

    pub fn paste(_text: &str, _restore: bool) -> Result<(), String> {
        Err("pasting is only available on macOS".into())
    }
}
