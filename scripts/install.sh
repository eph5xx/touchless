#!/bin/sh
# Builds Touchless, signs it and installs it to /Applications. Run it again to update.
#
# The app is signed with a stable local certificate ("Touchless Dev") so macOS keeps
# the Microphone, Accessibility and Input Monitoring permissions and the Keychain
# access across updates. The certificate is created on the first run.
set -eu

IDENTITY="Touchless Dev"
BUNDLE_ID="com.touchless.desktop"
APP="/Applications/Touchless.app"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

cd "$(dirname "$0")/.."

if ! cargo tauri --version >/dev/null 2>&1; then
  echo "Tauri CLI not found. Install it with: cargo install tauri-cli --version '^2' --locked" >&2
  exit 1
fi

has_identity() {
  security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$IDENTITY\""
}

create_identity() {
  echo "==> Creating the \"$IDENTITY\" code-signing certificate (one time)"
  tmp=$(mktemp -d)
  cat >"$tmp/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $IDENTITY
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$tmp/cert.cnf" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null
  /usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -name "$IDENTITY" -passout pass:touchless -out "$tmp/identity.p12"
  security import "$tmp/identity.p12" -k "$KEYCHAIN" -P touchless -T /usr/bin/codesign >/dev/null
  echo "==> macOS will ask for your password to trust the certificate for code signing"
  if ! security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$tmp/cert.pem"; then
    echo "warning: could not mark the certificate as trusted; signing may still work" >&2
  fi
  rm -rf "$tmp"
}

has_identity || create_identity

echo "==> Building Touchless"
# Tauri runs `xattr -cr` before signing; Homebrew's xattr shadows the system one
# and does not support -r, so prefer the system tools. Update archives are only
# built by the release workflow, which has the updater signing key.
APPLE_SIGNING_IDENTITY="$IDENTITY" PATH="/usr/bin:$PATH" cargo tauri build --bundles app \
  --config '{"bundle":{"createUpdaterArtifacts":false}}'
BUILT="target/release/bundle/macos/Touchless.app"

is_running() {
  [ "$(osascript -e "application id \"$BUNDLE_ID\" is running" 2>/dev/null)" = "true" ]
}

if is_running; then
  echo "==> Quitting the running Touchless"
  osascript -e "tell application id \"$BUNDLE_ID\" to quit"
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    is_running || break
    sleep 0.5
  done
fi

echo "==> Installing to $APP"
rm -rf "$APP"
ditto "$BUILT" "$APP"
open "$APP"
echo "==> Done. Touchless is in your menu bar."
