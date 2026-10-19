//! Read-only MCP server (stdio) over the Touchless transcript history.
//! stdout carries only MCP messages; diagnostics go to stderr.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone};
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use rusqlite::{Connection, OpenFlags, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const USAGE: &str = "Usage: touchless-mcp [--db <path to touchless.db>]";
const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListTranscripts {
    /// Inclusive start: RFC 3339 (`2026-10-18T09:00:00+02:00`), local datetime
    /// (`2026-10-18T09:00`) or local date (`2026-10-18`).
    from: Option<String>,
    /// Exclusive end, in the same formats as `from`.
    to: Option<String>,
    /// Page size, 1-100. Defaults to 20.
    limit: Option<u32>,
    /// `next_cursor` from the previous page. Keep `from` and `to` unchanged.
    cursor: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct Page {
    /// Newest first.
    transcripts: Vec<Transcript>,
    /// Pass as `cursor` to get the next (older) page; null on the last page.
    next_cursor: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct Transcript {
    id: i64,
    text: String,
    /// When dictation started, RFC 3339 in the Mac's local timezone.
    created_at: String,
    duration_ms: i64,
    plugin_results: Vec<PluginResult>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
struct PluginResult {
    plugin_id: String,
    plugin_name: String,
    /// The card the plugin produced, as stored.
    card: serde_json::Value,
    created_at: String,
}

#[derive(Debug, PartialEq)]
struct Query {
    from: Option<i64>,
    to: Option<i64>,
    /// `(created_at, id)` of the last item already returned.
    after: Option<(i64, i64)>,
    limit: u32,
}

impl Query {
    fn parse(args: ListTranscripts) -> Result<Self, String> {
        Ok(Self {
            from: args
                .from
                .as_deref()
                .map(|v| parse_time("from", v))
                .transpose()?,
            to: args
                .to
                .as_deref()
                .map(|v| parse_time("to", v))
                .transpose()?,
            after: args.cursor.as_deref().map(parse_cursor).transpose()?,
            limit: args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
}

/// Milliseconds since the epoch. Inputs without an offset use the local timezone.
fn parse_time(field: &str, value: &str) -> Result<i64, String> {
    let value = value.trim();
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Ok(time.timestamp_millis());
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .or_else(|_| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").map(|d| d.and_time(NaiveTime::MIN))
        })
        .map_err(|_| {
            format!(
                "`{field}` must be RFC 3339, YYYY-MM-DDTHH:MM[:SS] or YYYY-MM-DD, got {value:?}"
            )
        })?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|time| time.timestamp_millis())
        .ok_or_else(|| format!("`{field}` {value:?} does not exist in the local timezone"))
}

fn parse_cursor(cursor: &str) -> Result<(i64, i64), String> {
    cursor
        .split_once(':')
        .and_then(|(created_at, id)| Some((created_at.parse().ok()?, id.parse().ok()?)))
        .ok_or_else(|| {
            "Invalid `cursor`; pass `next_cursor` from the previous page unchanged".into()
        })
}

fn format_time(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map_or_else(
        || ms.to_string(),
        |time| time.to_rfc3339_opts(SecondsFormat::Millis, false),
    )
}

fn open(path: &Path) -> Result<Connection, String> {
    if !path.exists() {
        return Err(format!(
            "No Touchless database at {}. Open Touchless once to create it.",
            path.display()
        ));
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    // The app may be writing a new dictation; wait instead of failing.
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn list(connection: &Connection, query: &Query) -> rusqlite::Result<Page> {
    let (after_created_at, after_id) = query.after.unzip();
    let mut statement = connection.prepare(
        "SELECT id, text, created_at, duration_ms FROM transcripts
         WHERE (?1 IS NULL OR created_at >= ?1)
           AND (?2 IS NULL OR created_at < ?2)
           AND (?3 IS NULL OR created_at < ?3 OR (created_at = ?3 AND id < ?4))
         ORDER BY created_at DESC, id DESC
         LIMIT ?5",
    )?;
    // One extra row tells whether another page exists.
    let mut rows = statement
        .query_map(
            params![
                query.from,
                query.to,
                after_created_at,
                after_id,
                query.limit + 1
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > query.limit as usize;
    rows.truncate(query.limit as usize);
    let next_cursor = has_more
        .then(|| {
            rows.last()
                .map(|(id, _, created_at, _)| format!("{created_at}:{id}"))
        })
        .flatten();

    let mut plugin_statement = connection.prepare(
        "SELECT plugin_id, plugin_name, card, created_at FROM plugin_results
         WHERE transcript_id = ?1 ORDER BY id",
    )?;
    let transcripts = rows
        .into_iter()
        .map(|(id, text, created_at, duration_ms)| {
            let plugin_results = plugin_statement
                .query_map(params![id], |row| {
                    let card: String = row.get(2)?;
                    Ok(PluginResult {
                        plugin_id: row.get(0)?,
                        plugin_name: row.get(1)?,
                        card: serde_json::from_str(&card).unwrap_or(serde_json::Value::Null),
                        created_at: format_time(row.get(3)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Transcript {
                id,
                text,
                created_at: format_time(created_at),
                duration_ms,
                plugin_results,
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Page {
        transcripts,
        next_cursor,
    })
}

#[derive(Clone)]
struct Touchless {
    db: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Touchless {
    fn new(db: PathBuf) -> Self {
        Self {
            db,
            tool_router: Self::tool_router(),
        }
    }

    /// List Touchless voice dictation transcripts, newest first, with their full
    /// text, duration and plugin results (such as English Teacher cards).
    /// Filter by time with `from` (inclusive) and `to` (exclusive); times without
    /// an offset use the Mac's local timezone. When `next_cursor` is not null,
    /// call again with it as `cursor` and the same `from`/`to` to get older items.
    #[tool(annotations(title = "List transcripts", read_only_hint = true))]
    async fn list_transcripts(
        &self,
        Parameters(args): Parameters<ListTranscripts>,
    ) -> Result<Json<Page>, String> {
        let query = Query::parse(args)?;
        let connection = open(&self.db)?;
        list(&connection, &query)
            .map(Json)
            .map_err(|error| format!("Could not read transcripts: {error}"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Touchless {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("touchless", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Read-only access to the user's Touchless dictation history. \
                 The Mac's local time is {}.",
                Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
            ))
    }
}

enum Command {
    Serve(PathBuf),
    Help,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut db = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            "-h" | "--help" => return Ok(Command::Help),
            _ => return Err(format!("Unknown argument {arg:?}\n{USAGE}")),
        }
    }
    match db {
        Some(db) => Ok(Command::Serve(db)),
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set; pass --db <path>")?;
            Ok(Command::Serve(PathBuf::from(home).join(
                "Library/Application Support/com.touchless.desktop/touchless.db",
            )))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let db = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Serve(db)) => db,
        Ok(Command::Help) => {
            eprintln!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let result = async {
        Touchless::new(db).serve(stdio()).await?.waiting().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("touchless-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE transcripts (
                    id INTEGER PRIMARY KEY,
                    text TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    duration_ms INTEGER NOT NULL
                );
                CREATE TABLE plugin_results (
                    id INTEGER PRIMARY KEY,
                    transcript_id INTEGER NOT NULL,
                    plugin_id TEXT NOT NULL,
                    plugin_name TEXT NOT NULL,
                    card TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );",
            )
            .unwrap();
        connection
    }

    fn insert(connection: &Connection, text: &str, created_at: i64) -> i64 {
        connection
            .execute(
                "INSERT INTO transcripts (text, created_at, duration_ms) VALUES (?1, ?2, 500)",
                params![text, created_at],
            )
            .unwrap();
        connection.last_insert_rowid()
    }

    fn query(args: ListTranscripts) -> Query {
        Query::parse(args).unwrap()
    }

    fn texts(page: &Page) -> Vec<&str> {
        page.transcripts.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn lists_newest_first_with_plugin_results() {
        let connection = db();
        let first = insert(&connection, "first", 1_000);
        insert(&connection, "second", 2_000);
        connection
            .execute(
                "INSERT INTO plugin_results (transcript_id, plugin_id, plugin_name, card, created_at)
                 VALUES (?1, 'english_teacher', 'English Teacher', '{\"tone\":\"tip\"}', 1100),
                        (?1, 'broken', 'Broken', 'not json', 1200)",
                params![first],
            )
            .unwrap();

        let page = list(&connection, &query(ListTranscripts::default())).unwrap();
        assert_eq!(texts(&page), ["second", "first"]);
        assert_eq!(page.next_cursor, None);
        assert!(page.transcripts[0].plugin_results.is_empty());
        let results = &page.transcripts[1].plugin_results;
        assert_eq!(results[0].card, serde_json::json!({ "tone": "tip" }));
        assert_eq!(results[0].created_at, format_time(1_100));
        assert_eq!(results[1].card, serde_json::Value::Null);
        assert_eq!(page.transcripts[1].created_at, format_time(1_000));
        assert_eq!(page.transcripts[1].duration_ms, 500);
    }

    #[test]
    fn from_is_inclusive_and_to_is_exclusive() {
        let connection = db();
        for (text, ms) in [("a", 1_000), ("b", 2_000), ("c", 3_000)] {
            insert(&connection, text, ms);
        }
        let page = list(
            &connection,
            &query(ListTranscripts {
                from: Some("1970-01-01T00:00:02Z".into()),
                to: Some("1970-01-01T00:00:03Z".into()),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(texts(&page), ["b"]);
    }

    #[test]
    fn cursor_pages_through_every_item_once() {
        let connection = db();
        for (text, ms) in [
            ("a", 1_000),
            ("b", 2_000),
            ("c", 2_000),
            ("d", 2_000),
            ("e", 3_000),
        ] {
            insert(&connection, text, ms);
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = list(
                &connection,
                &query(ListTranscripts {
                    from: Some("1970-01-01T00:00:01.5Z".into()),
                    limit: Some(2),
                    cursor: cursor.clone(),
                    ..Default::default()
                }),
            )
            .unwrap();
            seen.extend(page.transcripts.iter().map(|t| t.text.clone()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, ["e", "d", "c", "b"]);
    }

    #[test]
    fn clamps_limit() {
        assert_eq!(
            query(ListTranscripts {
                limit: Some(0),
                ..Default::default()
            })
            .limit,
            1
        );
        assert_eq!(
            query(ListTranscripts {
                limit: Some(1_000),
                ..Default::default()
            })
            .limit,
            100
        );
        assert_eq!(query(ListTranscripts::default()).limit, DEFAULT_LIMIT);
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("from", "1970-01-01T00:00:01Z"), Ok(1_000));
        assert_eq!(parse_time("from", "1970-01-01T02:00:01.5+02:00"), Ok(1_500));
        let local = |y, m, d, h, min, s| {
            Local
                .with_ymd_and_hms(y, m, d, h, min, s)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(
            parse_time("from", "2026-10-18"),
            Ok(local(2026, 10, 18, 0, 0, 0))
        );
        assert_eq!(
            parse_time("from", "2026-10-18T09:30"),
            Ok(local(2026, 10, 18, 9, 30, 0))
        );
        assert_eq!(
            parse_time("from", "2026-10-18T09:30:15"),
            Ok(local(2026, 10, 18, 9, 30, 15))
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(
            Query::parse(ListTranscripts {
                from: Some("yesterday".into()),
                ..Default::default()
            })
            .unwrap_err()
            .contains("`from`")
        );
        assert!(
            Query::parse(ListTranscripts {
                cursor: Some("nope".into()),
                ..Default::default()
            })
            .unwrap_err()
            .contains("cursor")
        );
    }

    #[test]
    fn reports_missing_database() {
        let error = open(Path::new("/nonexistent/touchless.db")).unwrap_err();
        assert!(error.contains("Open Touchless once"));
    }
}
