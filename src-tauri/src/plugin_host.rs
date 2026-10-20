//! Runs plugins in the background after each dictation, lends them the app's
//! instruments, shows their cards at the top-right and saves them to history.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    db::{Db, PluginResult},
    openai::OpenAi,
    plugin_api::{Block, BoxFuture, Card, Dictation, Instrument, JsonRequest, Llm, Plugin, Tools},
    settings::{self, Provider, Settings},
    tips::Tips,
};

const PLUGINS_KEY: &str = "plugins";
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredPlugins {
    enabled: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    enabled: bool,
    /// Instruments the plugin needs that are not set up yet.
    missing: Vec<Instrument>,
}

pub struct PluginHost {
    plugins: Vec<Arc<dyn Plugin>>,
    enabled: Mutex<BTreeSet<String>>,
}

impl PluginHost {
    pub fn load(db: &Db, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        let stored: StoredPlugins = settings::load_json(db, PLUGINS_KEY);
        Self {
            plugins,
            enabled: Mutex::new(stored.enabled),
        }
    }

    pub fn view(&self, available: &[Instrument]) -> Vec<PluginView> {
        let enabled = self.enabled.lock().unwrap();
        self.plugins
            .iter()
            .map(|plugin| {
                let manifest = plugin.manifest();
                PluginView {
                    id: manifest.id,
                    name: manifest.name,
                    description: manifest.description,
                    enabled: enabled.contains(manifest.id),
                    missing: missing(manifest.needs, available),
                }
            })
            .collect()
    }

    pub fn set_enabled(&self, db: &Db, id: &str, enabled: bool) -> Result<(), String> {
        if !self.plugins.iter().any(|plugin| plugin.manifest().id == id) {
            return Err(format!("there is no plugin \"{id}\""));
        }
        let mut current = self.enabled.lock().unwrap();
        let mut next = current.clone();
        if enabled {
            next.insert(id.to_owned());
        } else {
            next.remove(id);
        }
        settings::save_json(
            db,
            PLUGINS_KEY,
            &StoredPlugins {
                enabled: next.clone(),
            },
        )
        .map_err(|error| format!("could not save plugins: {error}"))?;
        *current = next;
        Ok(())
    }

    /// Enabled plugins whose instruments are set up and that want this dictation.
    pub fn applicable(
        &self,
        available: &[Instrument],
        dictation: &Dictation,
    ) -> Vec<Arc<dyn Plugin>> {
        let enabled = self.enabled.lock().unwrap();
        self.plugins
            .iter()
            .filter(|plugin| {
                let manifest = plugin.manifest();
                enabled.contains(manifest.id)
                    && missing(manifest.needs, available).is_empty()
                    && plugin.wants(dictation)
            })
            .cloned()
            .collect()
    }
}

/// Instruments that are set up right now.
pub fn available(settings: &Settings) -> Vec<Instrument> {
    let mut instruments = Vec::new();
    if settings.api_key(Provider::OpenAi).is_some() {
        instruments.push(Instrument::Llm);
    }
    instruments
}

fn missing(needs: &[Instrument], available: &[Instrument]) -> Vec<Instrument> {
    needs
        .iter()
        .copied()
        .filter(|need| !available.contains(need))
        .collect()
}

struct Unavailable(String);

impl Llm for Unavailable {
    fn json(&self, _request: JsonRequest) -> BoxFuture<'_, Result<serde_json::Value, String>> {
        let error = self.0.clone();
        Box::pin(async move { Err(error) })
    }
}

/// Runs `plugins` on a dictation; each card shows as soon as its plugin answers.
pub async fn run(app: &AppHandle, plugins: Vec<Arc<dyn Plugin>>, dictation: Dictation) {
    let openai_key = app.state::<Settings>().api_key(Provider::OpenAi);
    let llm: Box<dyn Llm> = match openai_key.map(OpenAi::new) {
        Some(Ok(client)) => Box::new(client),
        Some(Err(error)) => Box::new(Unavailable(error)),
        None => Box::new(Unavailable("Add an OpenAI key in Settings".into())),
    };
    let tools = Tools { llm: llm.as_ref() };

    let saved = join_all(
        plugins
            .iter()
            .map(|plugin| run_one(app, plugin.as_ref(), &dictation, &tools)),
    )
    .await;
    if saved.contains(&true) {
        let _ = app.emit("transcripts-changed", ());
    }
}

/// Returns whether the plugin produced a card.
async fn run_one(
    app: &AppHandle,
    plugin: &dyn Plugin,
    dictation: &Dictation,
    tools: &Tools<'_>,
) -> bool {
    let manifest = plugin.manifest();
    let outcome = tokio::time::timeout(PLUGIN_TIMEOUT, plugin.on_dictation(dictation, tools))
        .await
        .unwrap_or_else(|_| Err("took too long to answer".into()));
    let card = match outcome {
        Ok(Some(card)) => card,
        Ok(None) => return false,
        // Errors stay out of sight so a flaky network never interrupts work.
        Err(error) => {
            eprintln!("{} failed: {error}", manifest.name);
            return false;
        }
    };

    let result = PluginResult {
        plugin_id: manifest.id.to_owned(),
        plugin_name: manifest.name.to_owned(),
        card: serde_json::to_value(&card).unwrap_or_default(),
        created_at: now_ms(),
    };
    if let Err(error) = app
        .state::<Db>()
        .insert_plugin_result(dictation.transcript_id, &result)
    {
        eprintln!("could not save the {} result: {error}", manifest.name);
    }
    app.state::<Tips>()
        .show(&card, display_time(&card), dictation.transcript_id);
    true
}

/// Long enough to glance at the card, short enough not to linger.
pub fn display_time(card: &Card) -> Duration {
    let chars: usize = card
        .blocks
        .iter()
        .map(|block| match block {
            Block::Text { text } => text.chars().count(),
            Block::Change { before, after } => before.chars().count() + after.chars().count(),
        })
        .sum();
    Duration::from_millis((3_000 + 50 * chars as u64).clamp(4_000, 7_000))
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_api::Manifest;

    struct Fake;

    static FAKE: Manifest = Manifest {
        id: "fake",
        name: "Fake",
        description: "",
        needs: &[Instrument::Llm],
    };

    impl Plugin for Fake {
        fn manifest(&self) -> &'static Manifest {
            &FAKE
        }

        fn wants(&self, dictation: &Dictation) -> bool {
            dictation.text != "skip"
        }

        fn on_dictation<'a>(
            &'a self,
            _dictation: &'a Dictation,
            _tools: &'a Tools<'a>,
        ) -> BoxFuture<'a, Result<Option<Card>, String>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn host(db: &Db) -> PluginHost {
        PluginHost::load(db, vec![Arc::new(Fake)])
    }

    fn dictation(text: &str) -> Dictation {
        Dictation {
            transcript_id: 1,
            text: text.into(),
            language: None,
        }
    }

    #[test]
    fn plugins_start_disabled_and_remember_being_enabled() {
        let db = Db::open_in_memory().unwrap();
        assert!(!host(&db).view(&[])[0].enabled);

        host(&db).set_enabled(&db, "fake", true).unwrap();
        assert!(host(&db).view(&[])[0].enabled);

        host(&db).set_enabled(&db, "fake", false).unwrap();
        assert!(!host(&db).view(&[])[0].enabled);

        assert!(host(&db).set_enabled(&db, "nope", true).is_err());
    }

    #[test]
    fn reports_missing_instruments() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(host(&db).view(&[])[0].missing, [Instrument::Llm]);
        assert!(host(&db).view(&[Instrument::Llm])[0].missing.is_empty());
    }

    #[test]
    fn runs_only_enabled_ready_and_willing_plugins() {
        let db = Db::open_in_memory().unwrap();
        let host = host(&db);
        let ready = [Instrument::Llm];
        assert!(
            host.applicable(&ready, &dictation("hello there"))
                .is_empty()
        );

        host.set_enabled(&db, "fake", true).unwrap();
        assert_eq!(host.applicable(&ready, &dictation("hello there")).len(), 1);
        assert!(host.applicable(&[], &dictation("hello there")).is_empty());
        assert!(host.applicable(&ready, &dictation("skip")).is_empty());
    }

    #[test]
    fn shows_cards_long_enough_to_glance_at() {
        let card = |text: &str| Card {
            tone: crate::plugin_api::Tone::Tip,
            blocks: vec![
                Block::Change {
                    before: "simple".into(),
                    after: "casual".into(),
                },
                Block::Text { text: text.into() },
            ],
        };
        let seconds = |card: Card| display_time(&card).as_secs_f64();
        assert_eq!(seconds(card("")), 4.0);
        assert_eq!(seconds(card("relaxed and informal")), 4.6);
        assert_eq!(seconds(card(&"x".repeat(500))), 7.0);
    }
}
