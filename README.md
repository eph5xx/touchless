<p align="center">
  <img src="icons/128x128@2x.png" width="128" alt="Touchless">
</p>

<h1 align="center">Touchless</h1>

<p align="center"><b>Talk instead of type. Anywhere on your Mac.</b></p>

Tap <kbd>⌥</kbd><kbd>⌘</kbd>, speak, tap again. Your words appear wherever your cursor is.

- **Fast and accurate**: real-time transcription by ElevenLabs Scribe.
- **Any language**: auto-detected, or pick your own.
- **Clean text**: drops the "um"s, and learns your names and jargon. Optional rewrites polish your wording (Enhance) or add paragraphs and lists (Structure).
- **Nothing lost**: every dictation is saved to history.
- **Plugins**: turn on English Teacher to get one grammar tip after each English dictation (needs an [OpenAI](https://platform.openai.com) key).

## Install

You need macOS, [Rust](https://rustup.rs) and an [ElevenLabs](https://elevenlabs.io) API key.

```sh
cargo install tauri-cli --version '^2' --locked
./scripts/install.sh
```

Open **Settings** from the menu bar icon, paste your API key, and allow the permissions macOS asks for.

To update, run `git pull && ./scripts/install.sh`.

## Use with local agents

Touchless ships a read-only [MCP](https://modelcontextprotocol.io) server that lets local agents read your dictation history over stdio. It has one tool, `list_transcripts`, which returns transcripts newest first with their full text and plugin results. It accepts an optional `from`/`to` time range, a `limit` and a `cursor` for the next page.

To connect Codex, Claude Code, Cursor, Factory Droid or Claude Desktop, open Touchless, go to **MCP** and click **Connect**. The page shows whether each app is connected and lets you update or disconnect it.

For other apps, add it to the app's MCP config:

```json
{
  "mcpServers": {
    "touchless": {
      "command": "/Applications/Touchless.app/Contents/MacOS/touchless-mcp"
    }
  }
}
```

It reads `~/Library/Application Support/com.touchless.desktop/touchless.db` by default; pass `--db <path>` to use another file.

## Development

```sh
cargo tauri dev   # run from source (quit the installed app first)
cargo test
cargo run --bin touchless-mcp -- --db <path>   # run the MCP server from source
```
