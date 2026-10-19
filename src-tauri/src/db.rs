use std::{path::Path, sync::Mutex};

use rusqlite::{
    Connection, OptionalExtension, ToSql, functions::FunctionFlags, params, types::ValueRef,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Transcript {
    pub id: i64,
    pub text: String,
    pub created_at: i64,
    pub duration_ms: i64,
    pub plugin_results: Vec<PluginResult>,
}

/// A card a plugin produced for a transcript.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PluginResult {
    pub plugin_id: String,
    pub plugin_name: String,
    pub card: serde_json::Value,
    pub created_at: i64,
}

pub const DEFAULT_PAGE_SIZE: u32 = 50;
pub const MAX_PAGE_SIZE: u32 = 100;

/// Where the next History page starts: just after this transcript.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct HistoryCursor {
    pub created_at: i64,
    pub id: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HistoryQuery {
    /// Matches transcript and card text as a substring, ignoring case.
    pub search: Option<String>,
    /// Earliest `created_at`, in milliseconds.
    pub from: Option<i64>,
    pub with_insights: bool,
    pub cursor: Option<HistoryCursor>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct HistoryPage {
    pub transcripts: Vec<Transcript>,
    pub next_cursor: Option<HistoryCursor>,
    /// Matching transcripts across all pages; only set on the first page.
    pub total: Option<i64>,
}

pub struct Db {
    connection: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> rusqlite::Result<Self> {
        // SQLite's own lower() and LIKE only fold ASCII; History search folds
        // the way JavaScript's toLowerCase() does.
        connection.create_scalar_function(
            "fold",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |context| {
                Ok(match context.get_raw(0) {
                    ValueRef::Text(text) => Some(String::from_utf8_lossy(text).to_lowercase()),
                    _ => None,
                })
            },
        )?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS transcripts (
                id INTEGER PRIMARY KEY,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plugin_results (
                id INTEGER PRIMARY KEY,
                transcript_id INTEGER NOT NULL,
                plugin_id TEXT NOT NULL,
                plugin_name TEXT NOT NULL,
                card TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS plugin_results_transcript
                ON plugin_results (transcript_id);
            CREATE INDEX IF NOT EXISTS transcripts_created
                ON transcripts (created_at, id);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert(
        &self,
        text: &str,
        created_at: i64,
        duration_ms: i64,
    ) -> rusqlite::Result<Transcript> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO transcripts (text, created_at, duration_ms) VALUES (?1, ?2, ?3)",
            params![text, created_at, duration_ms],
        )?;
        Ok(Transcript {
            id: connection.last_insert_rowid(),
            text: text.to_owned(),
            created_at,
            duration_ms,
            plugin_results: Vec::new(),
        })
    }

    pub fn insert_plugin_result(
        &self,
        transcript_id: i64,
        result: &PluginResult,
    ) -> rusqlite::Result<()> {
        let card = serde_json::to_string(&result.card)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO plugin_results (transcript_id, plugin_id, plugin_name, card, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                transcript_id,
                result.plugin_id,
                result.plugin_name,
                card,
                result.created_at
            ],
        )?;
        Ok(())
    }

    /// One page of History, newest first.
    pub fn list_page(&self, query: &HistoryQuery) -> rusqlite::Result<HistoryPage> {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE);
        let search = query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|search| !search.is_empty())
            .map(str::to_lowercase);

        let mut filters = Vec::new();
        let mut values: NamedValues = Vec::new();
        if let Some(from) = query.from {
            filters.push("t.created_at >= :from");
            values.push((":from", Box::new(from)));
        }
        if query.with_insights {
            filters.push("EXISTS (SELECT 1 FROM plugin_results p WHERE p.transcript_id = t.id)");
        }
        if let Some(search) = search {
            filters.push(
                "(instr(fold(t.text), :search) > 0 OR EXISTS (
                    SELECT 1 FROM plugin_results p, json_tree(p.card) j
                    WHERE p.transcript_id = t.id
                      AND j.key IN ('text', 'before', 'after')
                      AND j.type = 'text'
                      AND instr(fold(j.atom), :search) > 0))",
            );
            values.push((":search", Box::new(search)));
        }
        let count_where = if filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filters.join(" AND "))
        };

        let connection = self.connection.lock().unwrap();

        // Later pages keep the total the first page reported.
        let total = if query.cursor.is_none() {
            Some(connection.query_row(
                &format!("SELECT COUNT(*) FROM transcripts t {count_where}"),
                named(&values).as_slice(),
                |row| row.get::<_, i64>(0),
            )?)
        } else {
            None
        };

        let mut page_filters = filters;
        if let Some(cursor) = query.cursor {
            page_filters.push("(t.created_at, t.id) < (:cursor_created_at, :cursor_id)");
            values.push((":cursor_created_at", Box::new(cursor.created_at)));
            values.push((":cursor_id", Box::new(cursor.id)));
        }
        values.push((":limit", Box::new(limit + 1)));
        let page_where = if page_filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", page_filters.join(" AND "))
        };
        let mut statement = connection.prepare(&format!(
            "SELECT t.id, t.text, t.created_at, t.duration_ms FROM transcripts t
             {page_where}
             ORDER BY t.created_at DESC, t.id DESC
             LIMIT :limit"
        ))?;
        // One extra row tells whether another page exists.
        let mut transcripts = statement
            .query_map(named(&values).as_slice(), |row| {
                Ok(Transcript {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    created_at: row.get(2)?,
                    duration_ms: row.get(3)?,
                    plugin_results: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = transcripts.len() > limit as usize;
        transcripts.truncate(limit as usize);
        let next_cursor =
            has_more
                .then(|| transcripts.last())
                .flatten()
                .map(|last| HistoryCursor {
                    created_at: last.created_at,
                    id: last.id,
                });

        let mut results = connection.prepare_cached(
            "SELECT plugin_id, plugin_name, card, created_at FROM plugin_results
             WHERE transcript_id = ?1 ORDER BY id",
        )?;
        for transcript in &mut transcripts {
            transcript.plugin_results = results
                .query_map(params![transcript.id], |row| {
                    let card: String = row.get(2)?;
                    Ok(PluginResult {
                        plugin_id: row.get(0)?,
                        plugin_name: row.get(1)?,
                        card: serde_json::from_str(&card).unwrap_or(serde_json::Value::Null),
                        created_at: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }

        Ok(HistoryPage {
            transcripts,
            next_cursor,
            total,
        })
    }
    /// The text History shows first.
    pub fn latest_text(&self) -> rusqlite::Result<Option<String>> {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT text FROM transcripts ORDER BY created_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM plugin_results WHERE transcript_id = ?1",
            params![id],
        )?;
        transaction.execute("DELETE FROM transcripts WHERE id = ?1", params![id])?;
        transaction.commit()
    }

    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

type NamedValues<'a> = Vec<(&'a str, Box<dyn ToSql>)>;

fn named<'a>(values: &'a NamedValues<'_>) -> Vec<(&'a str, &'a dyn ToSql)> {
    values
        .iter()
        .map(|(name, value)| (*name, value.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(db: &Db, query: HistoryQuery) -> HistoryPage {
        db.list_page(&query).unwrap()
    }

    /// Every transcript the query matches, following cursors page by page.
    fn all_pages(db: &Db, query: HistoryQuery) -> Vec<Transcript> {
        let mut found = Vec::new();
        let mut cursor = None;
        loop {
            let next = page(
                db,
                HistoryQuery {
                    search: query.search.clone(),
                    cursor,
                    ..query
                },
            );
            found.extend(next.transcripts);
            match next.next_cursor {
                Some(next) => cursor = Some(next),
                None => return found,
            }
        }
    }

    fn texts(transcripts: &[Transcript]) -> Vec<&str> {
        transcripts
            .iter()
            .map(|transcript| transcript.text.as_str())
            .collect()
    }

    fn list(db: &Db) -> Vec<Transcript> {
        page(db, HistoryQuery::default()).transcripts
    }
    #[test]
    fn lists_newest_first() {
        let db = Db::open_in_memory().unwrap();
        db.insert("first", 1_000, 500).unwrap();
        let second = db.insert("second", 2_000, 700).unwrap();

        let all = list(&db);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], second);
        assert_eq!(all[1].text, "first");
    }

    #[test]
    fn deletes_transcripts() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert("first", 1_000, 500).unwrap();
        db.insert("second", 2_000, 700).unwrap();

        db.delete(first.id).unwrap();
        let all = list(&db);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, "second");
    }

    #[test]
    fn latest_text_follows_inserts_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.latest_text().unwrap(), None);
        db.insert("first", 1_000, 500).unwrap();
        let second = db.insert("second", 2_000, 700).unwrap();
        assert_eq!(db.latest_text().unwrap().as_deref(), Some("second"));

        db.delete(second.id).unwrap();
        assert_eq!(db.latest_text().unwrap().as_deref(), Some("first"));
    }

    fn lesson(created_at: i64) -> PluginResult {
        PluginResult {
            plugin_id: "english_teacher".into(),
            plugin_name: "English Teacher".into(),
            card: serde_json::json!({ "tone": "tip", "blocks": [] }),
            created_at,
        }
    }

    #[test]
    fn lists_plugin_results_with_their_transcript() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert("first", 1_000, 500).unwrap();
        let second = db.insert("second", 2_000, 700).unwrap();
        db.insert_plugin_result(first.id, &lesson(1_100)).unwrap();

        let all = list(&db);
        assert_eq!(all[0].id, second.id);
        assert!(all[0].plugin_results.is_empty());
        assert_eq!(all[1].plugin_results, [lesson(1_100)]);
    }

    #[test]
    fn deleting_a_transcript_deletes_its_plugin_results() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert("first", 1_000, 500).unwrap();
        db.insert_plugin_result(first.id, &lesson(1_100)).unwrap();
        db.delete(first.id).unwrap();

        // A reused id must not pick up the old results.
        let again = db.insert("again", 3_000, 500).unwrap();
        assert_eq!(again.id, first.id);
        assert!(list(&db)[0].plugin_results.is_empty());
    }

    #[test]
    fn pages_through_every_transcript_once() {
        let db = Db::open_in_memory().unwrap();
        // Several share a timestamp, so the cursor must break ties by id.
        for (index, created_at) in [1_000, 2_000, 2_000, 2_000, 3_000, 3_000, 4_000]
            .iter()
            .enumerate()
        {
            db.insert(&format!("t{index}"), *created_at, 0).unwrap();
        }

        let found = all_pages(
            &db,
            HistoryQuery {
                limit: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(texts(&found), ["t6", "t5", "t4", "t3", "t2", "t1", "t0"]);
        let first = page(
            &db,
            HistoryQuery {
                limit: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(first.total, Some(7));
        let second = page(
            &db,
            HistoryQuery {
                limit: Some(2),
                cursor: first.next_cursor,
                ..Default::default()
            },
        );
        assert_eq!(second.total, None);
    }

    #[test]
    fn caps_the_page_size() {
        let db = Db::open_in_memory().unwrap();
        for index in 0..120 {
            db.insert("text", index, 0).unwrap();
        }
        assert_eq!(page(&db, HistoryQuery::default()).transcripts.len(), 50);
        let big = page(
            &db,
            HistoryQuery {
                limit: Some(1_000),
                ..Default::default()
            },
        );
        assert_eq!(big.transcripts.len(), 100);
        assert!(big.next_cursor.is_some());
        assert_eq!(
            page(
                &db,
                HistoryQuery {
                    limit: Some(0),
                    ..Default::default()
                }
            )
            .transcripts
            .len(),
            1
        );
        let last = page(
            &db,
            HistoryQuery {
                cursor: big.next_cursor,
                ..Default::default()
            },
        );
        assert_eq!(last.transcripts.len(), 20);
        assert_eq!(last.next_cursor, None);
    }

    #[test]
    fn filters_by_date_and_insights() {
        let db = Db::open_in_memory().unwrap();
        let old = db.insert("old", 1_000, 0).unwrap();
        db.insert("new", 5_000, 0).unwrap();
        let newer = db.insert("newer", 6_000, 0).unwrap();
        db.insert_plugin_result(old.id, &lesson(1_100)).unwrap();
        db.insert_plugin_result(newer.id, &lesson(6_100)).unwrap();

        let recent = page(
            &db,
            HistoryQuery {
                from: Some(5_000),
                ..Default::default()
            },
        );
        assert_eq!(texts(&recent.transcripts), ["newer", "new"]);
        assert_eq!(recent.total, Some(2));

        let insights = page(
            &db,
            HistoryQuery {
                with_insights: true,
                ..Default::default()
            },
        );
        assert_eq!(texts(&insights.transcripts), ["newer", "old"]);
        assert_eq!(insights.total, Some(2));

        let both = page(
            &db,
            HistoryQuery {
                from: Some(5_000),
                with_insights: true,
                ..Default::default()
            },
        );
        assert_eq!(texts(&both.transcripts), ["newer"]);
        assert_eq!(both.total, Some(1));
    }

    #[test]
    fn searches_text_ignoring_case() {
        let db = Db::open_in_memory().unwrap();
        db.insert("Release plan for Friday", 1_000, 0).unwrap();
        db.insert("привет, мир", 2_000, 0).unwrap();
        db.insert("Größe", 3_000, 0).unwrap();
        db.insert("nothing here", 4_000, 0).unwrap();

        let search = |text: &str| {
            let result = page(
                &db,
                HistoryQuery {
                    search: Some(text.into()),
                    ..Default::default()
                },
            );
            assert_eq!(result.total, Some(result.transcripts.len() as i64));
            texts(&result.transcripts)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(search("RELEASE PLAN"), ["Release plan for Friday"]);
        assert_eq!(search("ПРИВЕТ"), ["привет, мир"]);
        assert_eq!(search("GRÖ"), ["Größe"]);
        assert_eq!(search("  friday "), ["Release plan for Friday"]);
        // `%` and `_` are plain characters, not wildcards.
        assert_eq!(search("%"), Vec::<String>::new());
        assert_eq!(search("").len(), 4);
    }

    #[test]
    fn searches_card_text() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert("I have went home", 1_000, 0).unwrap();
        let second = db.insert("Something else", 2_000, 0).unwrap();
        db.insert_plugin_result(
            first.id,
            &PluginResult {
                card: serde_json::json!({
                    "tone": "tip",
                    "blocks": [{ "kind": "change", "before": "have went", "after": "Went" }],
                }),
                ..lesson(1_100)
            },
        )
        .unwrap();
        db.insert_plugin_result(
            second.id,
            &PluginResult {
                card: serde_json::json!({
                    "tone": "tip",
                    "blocks": [{ "kind": "text", "text": "Use the Past Simple" }],
                }),
                ..lesson(2_100)
            },
        )
        .unwrap();

        let search = |text: &str| {
            texts(
                &page(
                    &db,
                    HistoryQuery {
                        search: Some(text.into()),
                        ..Default::default()
                    },
                )
                .transcripts,
            )
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        };
        assert_eq!(search("past simple"), ["Something else"]);
        assert_eq!(search("WENT"), ["I have went home"]);
        // Only the card's text is searched, not its keys or other fields.
        assert_eq!(search("tip"), Vec::<String>::new());
        assert_eq!(search("change"), Vec::<String>::new());
    }

    #[test]
    fn deleted_results_stay_out_of_search() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert("first", 1_000, 0).unwrap();
        db.insert_plugin_result(
            first.id,
            &PluginResult {
                card: serde_json::json!({ "blocks": [{ "kind": "text", "text": "secret tip" }] }),
                ..lesson(1_100)
            },
        )
        .unwrap();
        db.delete(first.id).unwrap();

        let again = db.insert("again", 3_000, 0).unwrap();
        assert_eq!(again.id, first.id);
        let found = page(
            &db,
            HistoryQuery {
                search: Some("secret".into()),
                ..Default::default()
            },
        );
        assert!(found.transcripts.is_empty());
        assert_eq!(found.total, Some(0));
        let insights = page(
            &db,
            HistoryQuery {
                with_insights: true,
                ..Default::default()
            },
        );
        assert!(insights.transcripts.is_empty());
    }

    #[test]
    fn stores_settings() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_setting("a").unwrap(), None);
        db.set_setting("a", "1").unwrap();
        db.set_setting("a", "2").unwrap();
        assert_eq!(db.get_setting("a").unwrap().as_deref(), Some("2"));
    }
}
