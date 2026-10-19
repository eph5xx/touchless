<p align="center">
  <img src="src-tauri/icons/128x128@2x.png" width="128" alt="Touchless">
</p>

<h1 align="center">Touchless</h1>

<p align="center">Tap <kbd>⌥</kbd><kbd>⌘</kbd>, speak, tap again.<br>Your words appear wherever your cursor is.</p>

## Install

You need macOS and [Rust](https://rustup.rs).

```sh
cargo install tauri-cli --version '^2' --locked
./scripts/install.sh
```

Touchless opens in your menu bar. To update, run `git pull && ./scripts/install.sh`.

## Get an ElevenLabs API key

ElevenLabs has a free plan, so you can try Touchless without paying.

1. Sign up at [elevenlabs.io](https://elevenlabs.io).
2. Open [Developers → API Keys](https://elevenlabs.io/app/developers/api-keys) and click **Create key**. Speech to Text access is all Touchless needs.
3. In Touchless, open **Settings**, paste the key under **ElevenLabs**, and allow the permissions macOS asks for.
