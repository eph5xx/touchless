<p align="center">
  <img src="icons/128x128@2x.png" width="128" alt="Touchless">
</p>

<h1 align="center">Touchless</h1>

<p align="center"><b>Talk instead of type. Anywhere on your Mac.</b></p>

Tap <kbd>⌥</kbd><kbd>⌘</kbd>, speak, tap again. Your words appear wherever your cursor is.

- **Fast and accurate**: real-time transcription by ElevenLabs Scribe.
- **Any language**: auto-detected, or pick your own.
- **Clean text**: drops the "um"s, and learns your names and jargon.
- **Nothing lost**: every dictation is saved to history.

## Install

You need macOS, [Rust](https://rustup.rs) and an [ElevenLabs](https://elevenlabs.io) API key.

```sh
cargo install tauri-cli --version '^2' --locked
./scripts/install.sh
```

Open **Settings** from the menu bar icon, paste your API key, and allow the permissions macOS asks for.

To update, run `git pull && ./scripts/install.sh`.

## Development

```sh
cargo tauri dev   # run from source (quit the installed app first)
cargo test
```
