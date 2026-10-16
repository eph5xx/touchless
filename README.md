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

You need an Apple Silicon Mac and an [ElevenLabs](https://elevenlabs.io) API key.

1. Download the `.dmg` from the [latest release](https://github.com/eph5xx/touchless/releases/latest) and drag Touchless to Applications.
2. Open Touchless. macOS blocks it the first time because the app isn't notarized. Go to **System Settings → Privacy & Security** and click **Open Anyway**.
3. Open **Settings** from the menu bar icon, paste your API key, and allow the permissions macOS asks for.

Touchless checks for new versions on its own. When one is out, click **Install and restart** in Settings or in the menu bar. Updates installed this way keep your permissions and skip the **Open Anyway** step.

## Build from source

You need macOS, [Rust](https://rustup.rs) and the Tauri CLI.

```sh
cargo install tauri-cli --version '^2' --locked
./scripts/install.sh
```

The script signs the app with a local "Touchless Dev" certificate, so macOS keeps the permissions across rebuilds. To update, run `git pull && ./scripts/install.sh`.

## Development

```sh
cargo tauri dev   # run from source (quit the installed app first)
cargo test
```

Development builds never check for or install updates.

## Releasing

Releases are built by the **Release** workflow on GitHub Actions and signed with the same "Touchless Dev" certificate, so updates keep the app's permissions.

One-time setup:

1. Create the updater key: `cargo tauri signer generate -w ~/.tauri/touchless.key`. Put the public key in `plugins.updater.pubkey` in `tauri.conf.json`. **Back up the private key and its password.** If you lose them, installed copies can never update again.
2. Export "Touchless Dev" from Keychain Access (My Certificates → right-click → Export → `.p12`, with a password).
3. Add the repository secrets:

   ```sh
   gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/touchless.key
   gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD   # paste the key password
   base64 -i TouchlessDev.p12 | gh secret set APPLE_CERTIFICATE
   gh secret set APPLE_CERTIFICATE_PASSWORD           # paste the .p12 password
   ```

For each release:

1. Bump `version` in `Cargo.toml` and merge to `main`.
2. Run the **Release** workflow (Actions → Release → Run workflow).
3. Check the draft release. The job summary shows the code signature; it should match `codesign -d -r- /Applications/Touchless.app`.
4. Publish the draft. Installed copies find it within a few hours, or right away with **Check now**.
