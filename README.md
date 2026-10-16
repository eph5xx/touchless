# Touchless

Voice-to-text for macOS. Tap <kbd>⌥</kbd><kbd>⌘</kbd>, talk, tap again: the text is transcribed by
[ElevenLabs](https://elevenlabs.io) Scribe, saved to history and pasted where your cursor is. You
can change the shortcut in **Settings**.

## Install

You need macOS, [Rust](https://rustup.rs) and the Tauri CLI:

```sh
cargo install tauri-cli --version '^2' --locked
./scripts/install.sh
```

The script builds the app, signs it with a local "Touchless Dev" certificate (created on the
first run; macOS asks for your password once), installs it to `/Applications` and starts it.

On first launch:

1. Open **Settings** from the menu bar icon and paste your ElevenLabs API key.
2. Allow Microphone, Input Monitoring and Accessibility when asked.

## Update

Get the latest code and run the script again:

```sh
git pull
./scripts/install.sh
```

Permissions and your API key carry over between updates.

## Development

```sh
cargo tauri dev   # run from source
cargo test        # run tests
```

Quit the installed app while running from source, so the shortcut isn't handled twice.
