use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Permissions {
    pub input_monitoring: bool,
    pub accessibility: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyPane {
    InputMonitoring,
    Accessibility,
}

pub fn current() -> Permissions {
    Permissions {
        input_monitoring: input_monitoring_granted(),
        accessibility: accessibility_granted(),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::{
        base::TCFType,
        boolean::CFBoolean,
        dictionary::{CFDictionary, CFDictionaryRef},
        string::{CFString, CFStringRef},
    };

    use super::PrivacyPane;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    pub fn input_monitoring_granted() -> bool {
        unsafe { CGPreflightListenEventAccess() }
    }

    pub fn request_input_monitoring() {
        unsafe { CGRequestListenEventAccess() };
    }

    pub fn accessibility_granted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    /// Shows the system prompt that deep-links to the Accessibility settings.
    pub fn request_accessibility() -> bool {
        let prompt_key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
        let options = CFDictionary::from_CFType_pairs(&[(prompt_key, CFBoolean::true_value())]);
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
    }

    pub fn open_privacy_settings(pane: PrivacyPane) -> Result<(), String> {
        let anchor = match pane {
            PrivacyPane::InputMonitoring => "Privacy_ListenEvent",
            PrivacyPane::Accessibility => "Privacy_Accessibility",
        };
        std::process::Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{anchor}"
            ))
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::PrivacyPane;

    pub fn input_monitoring_granted() -> bool {
        false
    }

    pub fn request_input_monitoring() {}

    pub fn accessibility_granted() -> bool {
        false
    }

    pub fn request_accessibility() -> bool {
        false
    }

    pub fn open_privacy_settings(_pane: PrivacyPane) -> Result<(), String> {
        Err("privacy settings are only available on macOS".into())
    }
}

pub use platform::*;
