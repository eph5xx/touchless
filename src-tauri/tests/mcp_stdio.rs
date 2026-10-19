//! Drives the `touchless-mcp` binary over stdio the way an agent would.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, channel},
    time::Duration,
};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start(db: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_touchless-mcp"))
            .arg("--db")
            .arg(db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(10)).unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }
}

fn create_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let connection = Connection::open(path).unwrap();
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
    for (text, created_at) in [("older", 1_000_i64), ("newer", 2_000)] {
        connection
            .execute(
                "INSERT INTO transcripts (text, created_at, duration_ms) VALUES (?1, ?2, 700)",
                params![text, created_at],
            )
            .unwrap();
    }
}

#[test]
fn lists_transcripts_over_stdio() {
    let db = std::env::temp_dir().join(format!("touchless-mcp-{}.db", std::process::id()));
    create_db(&db);
    let mut server = Server::start(&db);

    let init = server.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "touchless", "{init}");
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    server.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let list = server.request(2, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "{list}");
    assert_eq!(tools[0]["name"], "list_transcripts");
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert!(tools[0]["outputSchema"].is_object());

    let first = server.request(
        3,
        "tools/call",
        json!({ "name": "list_transcripts", "arguments": { "limit": 1 } }),
    );
    let page = &first["result"]["structuredContent"];
    assert_eq!(page["transcripts"][0]["text"], "newer", "{first}");
    assert_eq!(page["transcripts"][0]["duration_ms"], 700);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();

    let second = server.request(
        4,
        "tools/call",
        json!({ "name": "list_transcripts", "arguments": { "limit": 1, "cursor": cursor } }),
    );
    let page = &second["result"]["structuredContent"];
    assert_eq!(page["transcripts"][0]["text"], "older", "{second}");
    assert_eq!(page["next_cursor"], Value::Null);

    let bad = server.request(
        5,
        "tools/call",
        json!({ "name": "list_transcripts", "arguments": { "from": "yesterday" } }),
    );
    assert_eq!(bad["result"]["isError"], true, "{bad}");

    drop(server.stdin);
    assert!(server.child.wait().unwrap().success());
    let _ = std::fs::remove_file(&db);
}
