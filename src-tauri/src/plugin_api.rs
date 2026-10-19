//! The interface between Touchless and its plugins. Plugins live in `src/plugins/`
//! and use only what this module exports: the app runs them after each dictation
//! and hands them its instruments through [`Tools`].

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An app capability a plugin needs before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Instrument {
    /// A text model, reached through [`Tools::llm`]. Needs an OpenAI key.
    Llm,
}

pub struct Manifest {
    /// Stable identifier; saved settings and history results refer to it.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub needs: &'static [Instrument],
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
/// (the pop-up and History), so plugins need no UI code.
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

    /// A quick check of whether this dictation is worth handling. The pop-up shows
    /// a "checking" state only for plugins that return true.
    fn wants(&self, dictation: &Dictation) -> bool;

    /// `Ok(None)` means the plugin has nothing to say; nothing is shown or saved.
    fn on_dictation<'a>(
        &'a self,
        dictation: &'a Dictation,
        tools: &'a Tools<'a>,
    ) -> BoxFuture<'a, Result<Option<Card>, String>>;
}
