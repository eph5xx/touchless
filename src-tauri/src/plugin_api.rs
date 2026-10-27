//! The interface between Touchless and its plugins. Plugins live in `src/plugins/`
//! and use only what this module exports: the app runs them after each dictation
//! and hands them its instruments through [`Tools`].

use std::{collections::BTreeMap, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An app capability a plugin needs before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Instrument {
    /// A text model, reached through [`Tools::llm`]. Needs OpenAI, connected
    /// with an API key or a ChatGPT subscription.
    Llm,
}

pub struct Manifest {
    /// Stable identifier; saved settings and history results refer to it.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub needs: &'static [Instrument],
    /// Shown on the plugin's row while it is on; values reach
    /// [`Plugin::on_dictation`].
    pub settings: &'static [Setting],
}

#[derive(Debug, Serialize)]
pub struct Setting {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: SettingKind,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    /// A whole number picked with a slider.
    Range {
        min: i64,
        max: i64,
        step: i64,
        unit: &'static str,
        default: i64,
    },
    /// Any of `choices`, but at least one. The value is a list of choice ids.
    AnyOf {
        choices: &'static [Choice],
        default: &'static [&'static str],
    },
}

impl SettingKind {
    pub fn default_value(&self) -> serde_json::Value {
        match self {
            Self::Range { default, .. } => (*default).into(),
            Self::AnyOf { default, .. } => (*default).into(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

/// The current value of each setting, with defaults filled in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingValues(BTreeMap<String, serde_json::Value>);

impl SettingValues {
    pub fn set(&mut self, id: &str, value: serde_json::Value) {
        self.0.insert(id.to_owned(), value);
    }

    pub fn get(&self, id: &str) -> Option<&serde_json::Value> {
        self.0.get(id)
    }

    pub fn number(&self, id: &str) -> Option<i64> {
        self.0.get(id)?.as_i64()
    }

    /// The chosen ids of an [`SettingKind::AnyOf`] setting.
    pub fn chosen(&self, id: &str) -> Vec<String> {
        self.0
            .get(id)
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A finished dictation that was already pasted and saved to history.
#[derive(Debug, Clone)]
pub struct Dictation {
    pub transcript_id: i64,
    pub text: String,
    /// ISO 639-1 or 639-3 code of the spoken language, when known.
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Something to learn or act on.
    Tip,
    /// All good, nothing to do.
    Success,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    /// Shown as `before` struck through, then `after`.
    Change {
        before: String,
        after: String,
    },
}

/// What a plugin has to say about a dictation. The app decides where it is shown
/// (a card at the top-right of the screen and History), so plugins need no UI code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub tone: Tone,
    pub blocks: Vec<Block>,
}

/// A prompt whose answer must match `schema` (a strict JSON Schema object).
pub struct JsonRequest {
    pub instructions: String,
    pub input: String,
    pub schema_name: String,
    pub schema: serde_json::Value,
}

pub trait Llm: Send + Sync {
    fn json(&self, request: JsonRequest) -> BoxFuture<'_, Result<serde_json::Value, String>>;
}

/// Instruments the app lends to a plugin for one run.
pub struct Tools<'a> {
    pub llm: &'a dyn Llm,
}

pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &'static Manifest;

    /// A quick check of whether this dictation is worth handling. Only plugins
    /// that return true run.
    fn wants(&self, dictation: &Dictation) -> bool;

    /// `Ok(None)` means the plugin has nothing to say; nothing is shown or saved.
    fn on_dictation<'a>(
        &'a self,
        dictation: &'a Dictation,
        settings: &'a SettingValues,
        tools: &'a Tools<'a>,
    ) -> BoxFuture<'a, Result<Option<Card>, String>>;

    /// A sample card shown at the top-right when the plugin is turned on, so
    /// people see where its cards will appear. Never saved to history.
    fn example(&self) -> Option<Card> {
        None
    }
}
