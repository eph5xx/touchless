use std::{path::Path, sync::Mutex};

use rusqlite::{Connection, params};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Transcript {
    pub id: i64,
    pub text: String,
    pub created_at: i64,
    pub duration_ms: i64,
}

pub struct Db {
    connection: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(connection: Connection) -> rusqlite::Result<Self> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS transcripts (
                id INTEGER PRIMARY KEY,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );",
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
        })
    }

    pub fn list(&self) -> rusqlite::Result<Vec<Transcript>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, text, created_at, duration_ms FROM transcripts
             ORDER BY created_at DESC, id DESC",
        )?;
        statement
            .query_map([], |row| {
                Ok(Transcript {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    created_at: row.get(2)?,
                    duration_ms: row.get(3)?,
                })
            })?
            .collect()
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
}
