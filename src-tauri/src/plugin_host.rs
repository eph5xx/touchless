//! Runs plugins in the background after each dictation, lends them the app's
//! instruments, shows their cards at the top-right and saves them to history.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    db::{Db, PluginResult},
    openai::OpenAi,
    plugin_api::{
        BoxFuture, Card, Dictation, Instrument, JsonRequest, Llm, Plugin, Setting, SettingKind,
        SettingValues, Tools,
    },
    settings::{self, Settings},
    tips::Tips,
};

const PLUGINS_KEY: &str = "plugins";
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(25);
/// The example is new to the reader, so it stays at least this long.
const EXAMPLE_MIN_TIME: Duration = Duration::from_secs(8);
const DEFAULT_CARD_SECONDS: i64 = 6;

/// Offered for every plugin, since the app is what shows the cards.
static CARD_DURATION: Setting = Setting {
    id: "card_duration",
    label: "Show for",
    hint: "How long tips stay on screen. Hovering pauses them.",
    kind: SettingKind::Range {
        min: 3,
        max: 30,
        step: 1,
        unit: "s",
        default: DEFAULT_CARD_SECONDS,
    },
};

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct StoredPlugins {
    enabled: BTreeSet<String>,
    /// Plugin id → setting id → value, as saved. Values that no longer fit
    /// their setting are ignored when read.
    settings: BTreeMap<String, BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    enabled: bool,
    /// Instruments the plugin needs that are not set up yet.
    missing: Vec<Instrument>,
    settings: Vec<SettingView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingView {
    #[serde(flatten)]
    setting: &'static Setting,
    value: Value,
}

pub struct PluginHost {
    plugins: Vec<Arc<dyn Plugin>>,
    stored: Mutex<StoredPlugins>,
}

impl PluginHost {
    pub fn load(db: &Db, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        Self {
            plugins,
            stored: Mutex::new(settings::load_json(db, PLUGINS_KEY)),
        }
    }

    pub fn view(&self, available: &[Instrument]) -> Vec<PluginView> {
        let stored = self.stored.lock().unwrap();
        self.plugins
            .iter()
            .map(|plugin| {
                let manifest = plugin.manifest();
                let values = values_of(&stored, plugin.as_ref());
                PluginView {
                    id: manifest.id,
                    name: manifest.name,
                    description: manifest.description,
                    enabled: stored.enabled.contains(manifest.id),
                    missing: missing(manifest.needs, available),
                    settings: all_settings(plugin.as_ref())
                        .map(|setting| SettingView {
                            setting,
                            value: values.get(setting.id).cloned().unwrap_or_default(),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Returns whether the plugin was just turned on.
    pub fn set_enabled(&self, db: &Db, id: &str, enabled: bool) -> Result<bool, String> {
        if self.find(id).is_none() {
            return Err(format!("there is no plugin \"{id}\""));
        }
        let mut current = self.stored.lock().unwrap();
        let mut next = current.clone();
        if enabled {
            next.enabled.insert(id.to_owned());
        } else {
            next.enabled.remove(id);
        }
        save(db, &next)?;
        let turned_on = enabled && !current.enabled.contains(id);
        *current = next;
        Ok(turned_on)
    }

    pub fn set_setting(
        &self,
        db: &Db,
        id: &str,
        setting_id: &str,
        value: &Value,
    ) -> Result<(), String> {
        let plugin = self
            .find(id)
            .ok_or_else(|| format!("there is no plugin \"{id}\""))?;
        let setting = all_settings(plugin.as_ref())
            .find(|setting| setting.id == setting_id)
            .ok_or_else(|| format!("{} has no setting \"{setting_id}\"", plugin.manifest().name))?;
        let value = normalize(&setting.kind, value)?;
        let mut current = self.stored.lock().unwrap();
        let mut next = current.clone();
        next.settings
            .entry(id.to_owned())
            .or_default()
            .insert(setting_id.to_owned(), value);
        save(db, &next)?;
        *current = next;
        Ok(())
    }

    /// The plugin's settings as they are now, defaults filled in.
    pub fn values(&self, plugin: &dyn Plugin) -> SettingValues {
        values_of(&self.stored.lock().unwrap(), plugin)
    }

    /// The plugin's name, sample card and how long to show it, if it has one.
    pub fn example(&self, id: &str) -> Option<(&'static str, Card, Duration)> {
        let plugin = self.find(id)?;
        let card = plugin.example()?;
        let duration = card_duration(&self.values(plugin.as_ref())).max(EXAMPLE_MIN_TIME);
        Some((plugin.manifest().name, card, duration))
    }

    fn find(&self, id: &str) -> Option<&Arc<dyn Plugin>> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest().id == id)
    }

    /// Enabled plugins whose instruments are set up and that want this dictation.
    pub fn applicable(
        &self,
        available: &[Instrument],
        dictation: &Dictation,
    ) -> Vec<Arc<dyn Plugin>> {
        let stored = self.stored.lock().unwrap();
        self.plugins
            .iter()
            .filter(|plugin| {
                let manifest = plugin.manifest();
                stored.enabled.contains(manifest.id)
                    && missing(manifest.needs, available).is_empty()
                    && plugin.wants(dictation)
            })
            .cloned()
            .collect()
    }
}

fn save(db: &Db, stored: &StoredPlugins) -> Result<(), String> {
    settings::save_json(db, PLUGINS_KEY, stored)
        .map_err(|error| format!("could not save plugins: {error}"))
}

fn all_settings(plugin: &dyn Plugin) -> impl Iterator<Item = &'static Setting> {
    plugin.manifest().settings.iter().chain(std::iter::once(&CARD_DURATION))
}

fn values_of(stored: &StoredPlugins, plugin: &dyn Plugin) -> SettingValues {
    let saved = stored.settings.get(plugin.manifest().id);
    let mut values = SettingValues::default();
    for setting in all_settings(plugin) {
        let value = saved
            .and_then(|saved| saved.get(setting.id))
            .and_then(|value| normalize(&setting.kind, value).ok())
            .unwrap_or_else(|| setting.kind.default_value());
        values.set(setting.id, value);
    }
    values
}

/// The value to save for `kind`, or why `value` does not fit it.
fn normalize(kind: &SettingKind, value: &Value) -> Result<Value, String> {
    match kind {
        SettingKind::Range { min, max, step, .. } => {
            let number = value.as_i64().ok_or("expected a whole number")?;
            if number < *min || number > *max || (number - min) % step != 0 {
                return Err(format!("expected a number from {min} to {max}"));
            }
            Ok(number.into())
        }
        SettingKind::AnyOf { choices, .. } => {
            let items = value.as_array().ok_or("expected a list")?;
            let mut chosen = Vec::new();
            for item in items {
                let id = item
                    .as_str()
                    .filter(|id| choices.iter().any(|choice| choice.id == *id))
                    .ok_or_else(|| format!("unknown choice {item}"))?;
                chosen.push(id);
            }
            if chosen.is_empty() {
                return Err("choose at least one".into());
            }
            Ok(choices
                .iter()
                .filter(|choice| chosen.contains(&choice.id))
                .map(|choice| choice.id)
                .collect::<Vec<_>>()
                .into())
        }
    }
}

pub fn card_duration(values: &SettingValues) -> Duration {
    let seconds = values
        .number(CARD_DURATION.id)
        .unwrap_or(DEFAULT_CARD_SECONDS);
    Duration::from_secs(seconds.max(0) as u64)
}

/// Instruments that are set up right now.
pub fn available(settings: &Settings) -> Vec<Instrument> {
    let mut instruments = Vec::new();
    if settings.openai_ready() {
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
    let llm: Box<dyn Llm> = match OpenAi::from_settings(&app.state::<Settings>()) {
        Ok(client) => Box::new(client),
        Err(error) => Box::new(Unavailable(error)),
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
    let values = app.state::<PluginHost>().values(plugin);
    let outcome = tokio::time::timeout(
        PLUGIN_TIMEOUT,
        plugin.on_dictation(dictation, &values, tools),
    )
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
        .show(&card, card_duration(&values), dictation.transcript_id);
    true
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
    use crate::plugin_api::{Choice, Manifest};
    use serde_json::json;

    struct Fake;

    static FAKE: Manifest = Manifest {
        id: "fake",
        name: "Fake",
        description: "",
        needs: &[Instrument::Llm],
        settings: &[Setting {
            id: "colors",
            label: "Colors",
            hint: "",
            kind: SettingKind::AnyOf {
                choices: &[
                    Choice {
                        id: "red",
                        label: "Red",
                        hint: "",
                    },
                    Choice {
                        id: "blue",
                        label: "Blue",
                        hint: "",
                    },
                ],
                default: &["red"],
            },
        }],
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
            _settings: &'a SettingValues,
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

        assert!(host(&db).set_enabled(&db, "fake", true).unwrap());
        assert!(host(&db).view(&[])[0].enabled);
        assert!(!host(&db).set_enabled(&db, "fake", true).unwrap());

        assert!(!host(&db).set_enabled(&db, "fake", false).unwrap());
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
    fn settings_start_at_their_defaults() {
        let db = Db::open_in_memory().unwrap();
        let host = host(&db);
        let view = &host.view(&[])[0];
        let settings: Vec<_> = view
            .settings
            .iter()
            .map(|view| (view.setting.id, view.value.clone()))
            .collect();
        assert_eq!(
            settings,
            [("colors", json!(["red"])), ("card_duration", json!(6))]
        );
        assert_eq!(card_duration(&host.values(&Fake)), Duration::from_secs(6));
    }

    #[test]
    fn remembers_valid_settings() {
        let db = Db::open_in_memory().unwrap();
        host(&db)
            .set_setting(&db, "fake", "card_duration", &json!(12))
            .unwrap();
        host(&db)
            .set_setting(&db, "fake", "colors", &json!(["blue", "red", "blue"]))
            .unwrap();

        let values = host(&db).values(&Fake);
        assert_eq!(card_duration(&values), Duration::from_secs(12));
        // Choices keep their declared order and appear once.
        assert_eq!(values.chosen("colors"), ["red", "blue"]);
    }

    #[test]
    fn rejects_settings_that_do_not_fit() {
        let db = Db::open_in_memory().unwrap();
        let host = host(&db);
        for (setting, value) in [
            ("card_duration", json!(2)),
            ("card_duration", json!(31)),
            ("card_duration", json!(6.5)),
            ("card_duration", json!("6")),
            ("colors", json!([])),
            ("colors", json!(["green"])),
            ("colors", json!("red")),
            ("size", json!(1)),
        ] {
            assert!(
                host.set_setting(&db, "fake", setting, &value).is_err(),
                "{setting} = {value}"
            );
        }
        assert!(
            host.set_setting(&db, "nope", "card_duration", &json!(6))
                .is_err()
        );
        let values = host.values(&Fake);
        assert_eq!(card_duration(&values), Duration::from_secs(6));
        assert_eq!(values.chosen("colors"), ["red"]);
    }

    #[test]
    fn reads_older_and_broken_saved_settings() {
        let db = Db::open_in_memory().unwrap();
        settings::save_json(&db, PLUGINS_KEY, &json!({ "enabled": ["fake"] })).unwrap();
        assert!(host(&db).view(&[])[0].enabled);
        assert_eq!(host(&db).values(&Fake).chosen("colors"), ["red"]);

        settings::save_json(
            &db,
            PLUGINS_KEY,
            &json!({
                "enabled": ["fake"],
                "settings": { "fake": { "card_duration": 99, "colors": ["blue"] } },
            }),
        )
        .unwrap();
        let values = host(&db).values(&Fake);
        assert_eq!(card_duration(&values), Duration::from_secs(6));
        assert_eq!(values.chosen("colors"), ["blue"]);
    }

    #[test]
    fn plugin_settings_are_well_formed() {
        for plugin in crate::plugins::all() {
            let name = plugin.manifest().name;
            let mut ids: Vec<_> = all_settings(plugin.as_ref()).map(|s| s.id).collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), count, "{name} repeats a setting id");

            for setting in all_settings(plugin.as_ref()) {
                let default = setting.kind.default_value();
                assert_eq!(
                    normalize(&setting.kind, &default),
                    Ok(default),
                    "{name}: {}",
                    setting.id
                );
                if let SettingKind::AnyOf { choices, .. } = &setting.kind {
                    let mut ids: Vec<_> = choices.iter().map(|c| c.id).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    assert_eq!(ids.len(), choices.len(), "{name}: {}", setting.id);
                }
            }
        }
    }

    #[test]
    fn lends_the_llm_when_the_selected_method_is_set_up() {
        use crate::{chatgpt::ChatGpt, settings::OpenAiMethod};

        let available_with = |key: Option<&str>, method, signed_in| {
            available(&Settings::for_test(
                key,
                method,
                ChatGpt::for_test(signed_in),
            ))
        };
        assert_eq!(
            available_with(Some("sk"), OpenAiMethod::ApiKey, false),
            [Instrument::Llm]
        );
        assert!(available_with(None, OpenAiMethod::ApiKey, true).is_empty());
        assert_eq!(
            available_with(None, OpenAiMethod::ChatGpt, true),
            [Instrument::Llm]
        );
        assert!(available_with(Some("sk"), OpenAiMethod::ChatGpt, false).is_empty());
    }
}
