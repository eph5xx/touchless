use std::{collections::BTreeMap, env, sync::Mutex};

use serde::{Deserialize, Serialize};

use crate::db::Db;

const PREFERENCES_KEY: &str = "preferences";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    ElevenLabs,
    OpenAi,
    Jev,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::ElevenLabs, Provider::OpenAi, Provider::Jev];

    fn name(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "ElevenLabs",
            Provider::OpenAi => "OpenAI",
            Provider::Jev => "Jev",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "Realtime speech-to-text (Scribe v2).",
            Provider::OpenAi => "Reserved for upcoming plugins, like the English tutor.",
            Provider::Jev => "Typesafe AI. Reserved for upcoming agent features.",
        }
    }

    fn env_var(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "ELEVENLABS_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Jev => "JEV_API_KEY",
        }
    }

    fn testable(self) -> bool {
        !matches!(self, Provider::Jev)
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    Keychain,
    Environment,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    id: Provider,
    name: &'static str,
    description: &'static str,
    hint: Option<String>,
    source: Option<KeySource>,
    testable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    providers: Vec<ProviderStatus>,
    preferences: Preferences,
}

/// API keys live in the macOS Keychain (one item holding all keys, so a dev build
/// triggers at most one access prompt); preferences live in SQLite.
pub struct Settings {
    keys: Mutex<BTreeMap<Provider, String>>,
    preferences: Mutex<Preferences>,
}

impl Settings {
    pub fn load(db: &Db) -> Self {
        let keys = keychain::load().unwrap_or_else(|error| {
            eprintln!("could not read API keys from the Keychain: {error}");
            BTreeMap::new()
        });
        let preferences = db
            .get_setting(PREFERENCES_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        Self {
            keys: Mutex::new(keys),
            preferences: Mutex::new(preferences),
        }
    }

    pub fn view(&self) -> SettingsView {
        let keys = self.keys.lock().unwrap();
        let providers = Provider::ALL
            .into_iter()
            .map(|provider| {
                let (key, source) = match keys.get(&provider) {
                    Some(key) => (Some(key.clone()), Some(KeySource::Keychain)),
                    None => match env_key(provider) {
                        Some(key) => (Some(key), Some(KeySource::Environment)),
                        None => (None, None),
                    },
                };
                ProviderStatus {
                    id: provider,
                    name: provider.name(),
                    description: provider.description(),
                    hint: key.as_deref().map(mask),
                    source,
                    testable: provider.testable(),
                }
            })
            .collect();
        SettingsView {
            providers,
            preferences: self.preferences(),
        }
    }

    pub fn api_key(&self, provider: Provider) -> Option<String> {
        let stored = self.keys.lock().unwrap().get(&provider).cloned();
        stored.or_else(|| env_key(provider))
    }

    pub fn set_api_key(&self, provider: Provider, key: &str) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() {
            return Err("the API key is empty".into());
        }
        self.change_keys(|keys| {
            keys.insert(provider, key.to_owned());
        })
    }

    pub fn remove_api_key(&self, provider: Provider) -> Result<(), String> {
        self.change_keys(|keys| {
            keys.remove(&provider);
        })
    }

    fn change_keys(
        &self,
        change: impl FnOnce(&mut BTreeMap<Provider, String>),
    ) -> Result<(), String> {
        let mut keys = self.keys.lock().unwrap();
        let mut next = keys.clone();
        change(&mut next);
        keychain::save(&next).map_err(|error| format!("could not update the Keychain: {error}"))?;
        *keys = next;
        Ok(())
    }

    pub fn preferences(&self) -> Preferences {
        *self.preferences.lock().unwrap()
    }

    pub fn set_preferences(&self, db: &Db, preferences: Preferences) -> Result<(), String> {
        let value = serde_json::to_string(&preferences).map_err(|error| error.to_string())?;
        db.set_setting(PREFERENCES_KEY, &value)
            .map_err(|error| format!("could not save preferences: {error}"))?;
        *self.preferences.lock().unwrap() = preferences;
        Ok(())
    }
}

fn env_key(provider: Provider) -> Option<String> {
    env::var(provider.env_var())
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "••••".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

pub async fn test_api_key(provider: Provider, key: &str) -> Result<String, String> {
    match provider {
        Provider::ElevenLabs => crate::elevenlabs::ElevenLabs::verify(key)
            .await
            .map(|()| "Connected to ElevenLabs".into())
            .map_err(|error| error.to_string()),
        Provider::OpenAi => {
            let response = reqwest::Client::new()
                .get("https://api.openai.com/v1/models")
                .bearer_auth(key)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .map_err(|error| format!("could not reach OpenAI: {error}"))?;
            match response.status() {
                status if status.is_success() => Ok("Connected to OpenAI".into()),
                status if status.as_u16() == 401 => Err("OpenAI rejected the key".into()),
                status => Err(format!("OpenAI responded with {status}")),
            }
        }
        Provider::Jev => Err("Testing Jev keys is not supported yet".into()),
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    use std::collections::BTreeMap;

    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::Provider;

    const SERVICE: &str = "com.touchless.desktop";
    const ACCOUNT: &str = "api-keys";
    const ITEM_NOT_FOUND: i32 = -25300;

    pub fn load() -> Result<BTreeMap<Provider, String>, String> {
        match get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(BTreeMap::new()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn save(keys: &BTreeMap<Provider, String>) -> Result<(), String> {
        if keys.is_empty() {
            return match delete_generic_password(SERVICE, ACCOUNT) {
                Err(error) if error.code() != ITEM_NOT_FOUND => Err(error.to_string()),
                _ => Ok(()),
            };
        }
        let json = serde_json::to_vec(keys).map_err(|error| error.to_string())?;
        set_generic_password(SERVICE, ACCOUNT, &json).map_err(|error| error.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    use std::collections::BTreeMap;

    use super::Provider;

    pub fn load() -> Result<BTreeMap<Provider, String>, String> {
        Ok(BTreeMap::new())
    }

    pub fn save(_keys: &BTreeMap<Provider, String>) -> Result<(), String> {
        Err("secure key storage is only available on macOS".into())
    }
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
    fn provider_ids_are_stable() {
        assert_eq!(
            serde_json::to_string(&Provider::ElevenLabs).unwrap(),
            r#""elevenlabs""#
        );
        assert_eq!(
            serde_json::to_string(&Provider::OpenAi).unwrap(),
            r#""openai""#
        );
    }
}
