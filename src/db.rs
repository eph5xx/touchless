use std::{collections::HashMap, path::Path, sync::Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

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
                ON plugin_results (transcript_id);",
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

    pub fn list(&self) -> rusqlite::Result<Vec<Transcript>> {
        let connection = self.connection.lock().unwrap();
        let mut results: HashMap<i64, Vec<PluginResult>> = HashMap::new();
        let mut statement = connection.prepare(
            "SELECT transcript_id, plugin_id, plugin_name, card, created_at FROM plugin_results
             ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            let card: String = row.get(3)?;
            Ok((
                row.get::<_, i64>(0)?,
                PluginResult {
                    plugin_id: row.get(1)?,
                    plugin_name: row.get(2)?,
                    card: serde_json::from_str(&card).unwrap_or(serde_json::Value::Null),
                    created_at: row.get(4)?,
                },
            ))
        })?;
        for row in rows {
            let (transcript_id, result) = row?;
            results.entry(transcript_id).or_default().push(result);
        }

        let mut statement = connection.prepare(
            "SELECT id, text, created_at, duration_ms FROM transcripts
             ORDER BY created_at DESC, id DESC",
        )?;
        statement
            .query_map([], |row| {
                let id = row.get(0)?;
                Ok(Transcript {
                    id,
                    text: row.get(1)?,
                    created_at: row.get(2)?,
                    duration_ms: row.get(3)?,
                    plugin_results: results.remove(&id).unwrap_or_default(),
                })
            })?
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_newest_first() {
        let db = Db::open_in_memory().unwrap();
        db.insert("first", 1_000, 500).unwrap();
        let second = db.insert("second", 2_000, 700).unwrap();

        let all = db.list().unwrap();
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
        let all = db.list().unwrap();
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

        let all = db.list().unwrap();
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
        assert!(db.list().unwrap()[0].plugin_results.is_empty());
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
