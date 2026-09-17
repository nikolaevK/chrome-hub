#!/bin/sh
# Builds ChromeHub in release mode and wraps it in a signed .app bundle.
#
#   ./make_app.sh            build dist/ChromeHub.app
#   ./make_app.sh --run      build and (re)launch it
#   ./make_app.sh --make-cert
#       (optional) create the signing certificate in your login keychain
#       instead, which asks for your password once.
#
# Signing identity: macOS ties the Screen Recording / Accessibility grants to
# the app's code-signing requirement. Ad-hoc signing makes that a per-build
# hash, so every rebuild silently revoked the grants. The first build
# therefore creates a self-signed "ChromeHub Dev Signing" certificate in a
# dedicated keychain (no dialogs, nothing added to your trust settings) and
# every build signs with it; the requirement is then stable and the grants
# survive rebuilds. The keychain password only guards that throwaway key.
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

IDENTITY="ChromeHub Dev Signing"
BUNDLE_ID="com.konstantin.chromehub"
KEYCHAIN="$HOME/Library/Keychains/ChromeHubSigning.keychain-db"
KEYCHAIN_PASS="chromehub"

# Writes a self-signed code-signing certificate + key as a .p12 to "$1".
make_p12() {
  ctmp=$(mktemp -d)   # sh functions share the caller's variables: keep this name distinct
  cat > "$ctmp/cs.cnf" <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = ChromeHub Dev Signing
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
CNF
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$ctmp/cs.cnf" -keyout "$ctmp/cs.key" -out "$ctmp/cs.pem" 2>/dev/null
  openssl pkcs12 -export -inkey "$ctmp/cs.key" -in "$ctmp/cs.pem" -name "$IDENTITY" -passout pass:chromehub -out "$1" -legacy
  cp "$ctmp/cs.pem" "$1.pem"
  rm -rf "$ctmp"
}

# Creates the dedicated signing keychain with a fresh identity if missing.
ensure_keychain() {
  if [ -f "$KEYCHAIN" ]; then
    security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
    return
  fi
  tmp=$(mktemp -d)
  make_p12 "$tmp/cs.p12"
  security create-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
  security set-keychain-settings "$KEYCHAIN"   # never auto-lock
  security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN"
  security import "$tmp/cs.p12" -k "$KEYCHAIN" -P chromehub -T /usr/bin/codesign >/dev/null
  # Let codesign use the key without a confirmation dialog.
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null
  rm -rf "$tmp"
  echo "Created signing keychain $KEYCHAIN"
}

if [ "${1:-}" = "--make-cert" ]; then
  tmp=$(mktemp -d)
  make_p12 "$tmp/cs.p12"
  K="$HOME/Library/Keychains/login.keychain-db"
  security import "$tmp/cs.p12" -k "$K" -P chromehub -T /usr/bin/codesign
  security add-trusted-cert -r trustRoot -p codeSign -k "$K" "$tmp/cs.p12.pem"
  rm -rf "$tmp"
  echo "Certificate installed. Now run: ./make_app.sh --run"
  echo "Then remove ChromeHub from Screen Recording and Accessibility in System Settings and grant it again once."
  exit 0
fi

cargo build --release

APP="dist/ChromeHub.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/chromehub "$APP/Contents/MacOS/ChromeHub"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>ChromeHub</string>
  <key>CFBundleDisplayName</key>     <string>Chrome Hub</string>
  <key>CFBundleIdentifier</key>      <string>$BUNDLE_ID</string>
  <key>CFBundleVersion</key>         <string>0.1.0</string>
  <key>CFBundleShortVersionString</key> <string>0.1.0</string>
  <key>CFBundleExecutable</key>      <string>ChromeHub</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>LSMinimumSystemVersion</key>  <string>14.0</string>
  <key>LSUIElement</key>             <true/>
  <key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
PLIST

if security find-identity -v -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
  codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
  echo "Built $APP (signed with '$IDENTITY' from the login keychain)"
else
  ensure_keychain
  codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" --identifier "$BUNDLE_ID" "$APP"
  echo "Built $APP (signed with '$IDENTITY' from $KEYCHAIN)"
fi

if [ "${1:-}" = "--run" ]; then
  pkill -x ChromeHub 2>/dev/null || true
  open "$APP"
  echo "Launched. Look for the window icon in the menu bar."
fi
