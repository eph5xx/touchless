use std::sync::Mutex;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::db::Db;

const PREFERENCES_KEY: &str = "preferences";
const TRANSCRIPTION_KEY: &str = "transcription";
pub const MAX_KEYTERMS: usize = 50;
pub const MAX_EDIT_INSTRUCTION_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub auto_paste: bool,
    pub restore_clipboard: bool,
    pub show_overlay: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            auto_paste: true,
            restore_clipboard: true,
            show_overlay: true,
        }
    }
}

/// Options passed to the ElevenLabs realtime API for every dictation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptionOptions {
    /// ISO-639 code; `None` lets ElevenLabs detect the language.
    pub language_code: Option<String>,
    pub secondary_languages: Vec<String>,
    /// Removes filler words, false starts and disfluencies.
    pub no_verbatim: bool,
    pub keyterms: Vec<String>,
    /// Natural-language instruction applied to each committed transcript; empty is off.
    pub transcript_edit: String,
}

impl TranscriptionOptions {
    pub fn normalized(self) -> Result<Self, String> {
        let language_code = match self.language_code.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(code) => Some(normalize_language(code)?),
        };

        let mut secondary_languages: Vec<String> = Vec::new();
        for code in &self.secondary_languages {
            let code = normalize_language(code)?;
            if language_code.as_ref() != Some(&code) && !secondary_languages.contains(&code) {
                secondary_languages.push(code);
            }
        }

        let mut keyterms: Vec<String> = Vec::new();
        for term in &self.keyterms {
            let term = term.split_whitespace().collect::<Vec<_>>().join(" ");
            let duplicate = keyterms
                .iter()
                .any(|existing| existing.to_lowercase() == term.to_lowercase());
            if !term.is_empty() && !duplicate {
                keyterms.push(term);
            }
        }
        if keyterms.len() > MAX_KEYTERMS {
            return Err(format!("Use at most {MAX_KEYTERMS} custom words"));
        }

        let transcript_edit = self.transcript_edit.trim().to_owned();
        if transcript_edit.chars().count() > MAX_EDIT_INSTRUCTION_CHARS {
            return Err(format!(
                "The rewrite instruction can be at most {MAX_EDIT_INSTRUCTION_CHARS} characters"
            ));
        }

        Ok(Self {
            language_code,
            secondary_languages,
            no_verbatim: self.no_verbatim,
            keyterms,
            transcript_edit,
        })
    }
}

fn normalize_language(code: &str) -> Result<String, String> {
    let code = code.trim().to_ascii_lowercase();
    if (2..=3).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_lowercase()) {
        Ok(code)
    } else {
        Err(format!("\"{code}\" is not a valid language code"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyStatus {
    saved: bool,
    hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    api_key: ApiKeyStatus,
    preferences: Preferences,
    transcription: TranscriptionOptions,
}

/// The ElevenLabs API key lives in the macOS Keychain; everything else lives in SQLite.
pub struct Settings {
    api_key: Mutex<Option<String>>,
    preferences: Mutex<Preferences>,
    transcription: Mutex<TranscriptionOptions>,
}

impl Settings {
    pub fn load(db: &Db) -> Self {
        let api_key = keychain::load().unwrap_or_else(|error| {
            eprintln!("could not read the API key from the Keychain: {error}");
            None
        });
        Self {
            api_key: Mutex::new(api_key),
            preferences: Mutex::new(load_json(db, PREFERENCES_KEY)),
            transcription: Mutex::new(load_json(db, TRANSCRIPTION_KEY)),
        }
    }

    pub fn view(&self) -> SettingsView {
        let key = self.api_key();
        SettingsView {
            api_key: ApiKeyStatus {
                saved: key.is_some(),
                hint: key.as_deref().map(mask),
            },
            preferences: self.preferences(),
            transcription: self.transcription(),
        }
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key.lock().unwrap().clone()
    }

    pub fn set_api_key(&self, key: &str) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() {
            return Err("the API key is empty".into());
        }
        self.change_api_key(Some(key.to_owned()))
    }

    pub fn remove_api_key(&self) -> Result<(), String> {
        self.change_api_key(None)
    }

    fn change_api_key(&self, next: Option<String>) -> Result<(), String> {
        let mut current = self.api_key.lock().unwrap();
        keychain::save(next.as_deref())
            .map_err(|error| format!("could not update the Keychain: {error}"))?;
        *current = next;
        Ok(())
    }

    pub fn preferences(&self) -> Preferences {
        *self.preferences.lock().unwrap()
    }

    pub fn set_preferences(&self, db: &Db, preferences: Preferences) -> Result<(), String> {
        save_json(db, PREFERENCES_KEY, &preferences)
            .map_err(|error| format!("could not save preferences: {error}"))?;
        *self.preferences.lock().unwrap() = preferences;
        Ok(())
    }

    pub fn transcription(&self) -> TranscriptionOptions {
        self.transcription.lock().unwrap().clone()
    }

    pub fn set_transcription(&self, db: &Db, options: TranscriptionOptions) -> Result<(), String> {
        let options = options.normalized()?;
        save_json(db, TRANSCRIPTION_KEY, &options)
            .map_err(|error| format!("could not save transcription settings: {error}"))?;
        *self.transcription.lock().unwrap() = options;
        Ok(())
    }
}

fn load_json<T: DeserializeOwned + Default>(db: &Db, key: &str) -> T {
    db.get_setting(key)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(db: &Db, key: &str, value: &T) -> Result<(), String> {
    let value = serde_json::to_string(value).map_err(|error| error.to_string())?;
    db.set_setting(key, &value)
        .map_err(|error| error.to_string())
}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "••••".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// Connects with the current options and describes what ElevenLabs accepted.
pub async fn test_api_key(key: &str, options: &TranscriptionOptions) -> Result<String, String> {
    let config = crate::elevenlabs::ElevenLabs::verify(key, options)
        .await
        .map_err(|error| error.to_string())?;
    Ok(describe_session(&config))
}

fn describe_session(config: &serde_json::Value) -> String {
    let mut parts = vec!["Connected to ElevenLabs".to_owned()];
    match config["language_code"].as_str() {
        Some(code) if !code.is_empty() => parts.push(format!("language: {code}")),
        _ => parts.push("language: auto".into()),
    }
    if let Some(keyterms) = config["keyterms"].as_array()
        && !keyterms.is_empty()
    {
        parts.push(format!("{} custom words", keyterms.len()));
    }
    if config["no_verbatim"].as_bool() == Some(true) {
        parts.push("filler words removed".into());
    }
    parts.join(" · ")
}

/// The Keychain item holds a JSON object so the format can grow; only the
/// ElevenLabs entry is used. Older builds also stored other providers' keys
/// there, and those are dropped on the next save.
#[cfg(target_os = "macos")]
mod keychain {
    use std::collections::BTreeMap;

    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const SERVICE: &str = "com.touchless.desktop";
    const ACCOUNT: &str = "api-keys";
    const ELEVENLABS: &str = "elevenlabs";
    const ITEM_NOT_FOUND: i32 = -25300;

    pub fn load() -> Result<Option<String>, String> {
        match get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => super::parse_stored_keys(&bytes),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn save(key: Option<&str>) -> Result<(), String> {
        let Some(key) = key else {
            return match delete_generic_password(SERVICE, ACCOUNT) {
                Err(error) if error.code() != ITEM_NOT_FOUND => Err(error.to_string()),
                _ => Ok(()),
            };
        };
        let keys = BTreeMap::from([(ELEVENLABS, key)]);
        let json = serde_json::to_vec(&keys).map_err(|error| error.to_string())?;
        set_generic_password(SERVICE, ACCOUNT, &json).map_err(|error| error.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    pub fn load() -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn save(_key: Option<&str>) -> Result<(), String> {
        Err("secure key storage is only available on macOS".into())
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_stored_keys(bytes: &[u8]) -> Result<Option<String>, String> {
    let keys: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    Ok(keys
        .get("elevenlabs")
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keys() {
        assert_eq!(mask("sk_1234567890abcd"), "••••abcd");
        assert_eq!(mask("short"), "••••");
    }

    #[test]
    fn preferences_fill_missing_fields_with_defaults() {
        let preferences: Preferences = serde_json::from_str(r#"{"auto_paste":false}"#).unwrap();
        assert!(!preferences.auto_paste);
        assert!(preferences.restore_clipboard);
        assert!(preferences.show_overlay);
    }

    #[test]
    fn reads_elevenlabs_key_from_legacy_keychain_data() {
        let stored = br#"{"elevenlabs":"sk_eleven","openai":"sk_openai","jev":"x"}"#;
        assert_eq!(
            parse_stored_keys(stored).unwrap().as_deref(),
            Some("sk_eleven")
        );
        assert_eq!(parse_stored_keys(br#"{"openai":"sk"}"#).unwrap(), None);
    }

    #[test]
    fn transcription_options_fill_missing_fields_with_defaults() {
        let options: TranscriptionOptions =
            serde_json::from_str(r#"{"no_verbatim":true}"#).unwrap();
        assert!(options.no_verbatim);
        assert_eq!(options.language_code, None);
        assert!(options.keyterms.is_empty());
    }

    #[test]
    fn normalizes_transcription_options() {
        let options = TranscriptionOptions {
            language_code: Some(" ENG ".into()),
            secondary_languages: vec!["rus".into(), "eng".into(), "RUS".into(), "ukr".into()],
            no_verbatim: true,
            keyterms: vec![
                "  Touchless ".into(),
                "".into(),
                "touchless".into(),
                "Scribe   v2".into(),
            ],
            transcript_edit: "  Fix grammar.  ".into(),
        }
        .normalized()
        .unwrap();

        assert_eq!(options.language_code.as_deref(), Some("eng"));
        assert_eq!(options.secondary_languages, ["rus", "ukr"]);
        assert_eq!(options.keyterms, ["Touchless", "Scribe v2"]);
        assert_eq!(options.transcript_edit, "Fix grammar.");
    }

    #[test]
    fn empty_language_means_auto_detect() {
        let options = TranscriptionOptions {
            language_code: Some(" ".into()),
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(options.language_code, None);
    }

    #[test]
    fn rejects_invalid_transcription_options() {
        let invalid_language = TranscriptionOptions {
            language_code: Some("english".into()),
            ..Default::default()
        };
        assert!(invalid_language.normalized().is_err());

        let too_many_keyterms = TranscriptionOptions {
            keyterms: (0..=MAX_KEYTERMS)
                .map(|index| format!("term {index}"))
                .collect(),
            ..Default::default()
        };
        assert!(too_many_keyterms.normalized().is_err());

        let long_instruction = TranscriptionOptions {
            transcript_edit: "a".repeat(MAX_EDIT_INSTRUCTION_CHARS + 1),
            ..Default::default()
        };
        assert!(long_instruction.normalized().is_err());
    }

    #[test]
    fn describes_accepted_session() {
        let config = serde_json::json!({
            "language_code": "eng",
            "keyterms": ["a", "b"],
            "no_verbatim": true,
        });
        assert_eq!(
            describe_session(&config),
            "Connected to ElevenLabs · language: eng · 2 custom words · filler words removed"
        );
        assert_eq!(
            describe_session(&serde_json::json!({})),
            "Connected to ElevenLabs · language: auto"
        );
    }
}
