#!/usr/bin/env bash
# Assembles MicBridge.app from an already-built release binary.
#
# This is not packaging polish. A bare Mach-O binary cannot properly ask for
# microphone access: TCC attributes the request to the *bundle*, so a loose
# binary run from a terminal silently inherits the terminal's permission — which
# is why development has worked — and the same binary double-clicked from Finder
# captures zeros with no dialog and no error. NSMicrophoneUsageDescription in an
# Info.plist is what makes the request attributable, and the string is what the
# dialog shows the user.
#
#   scripts/bundle-macos.sh [target-dir]
#
# `target-dir` defaults to target/release. The universal build in CI passes the
# directory holding its lipo'd binaries instead.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN_DIR="${1:-target/release}"
APP="dist/MicBridge.app"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

for bin in micbridge micbridge-gui; do
    if [ ! -f "$BIN_DIR/$bin" ]; then
        echo "error: $BIN_DIR/$bin not found — run scripts/build-macos.sh first" >&2
        exit 1
    fi
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Both binaries: the window is what a user double-clicks, and the CLI is what
# they need over SSH or Remote Desktop, so shipping only one guarantees the
# other is missing exactly when it is wanted.
cp "$BIN_DIR/micbridge-gui" "$APP/Contents/MacOS/micbridge-gui"
cp "$BIN_DIR/micbridge" "$APP/Contents/MacOS/micbridge"
cp assets/micbridge.icns "$APP/Contents/Resources/micbridge.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                    <string>MicBridge</string>
    <key>CFBundleDisplayName</key>             <string>MicBridge</string>
    <key>CFBundleIdentifier</key>              <string>io.github.dieterpl.micbridge</string>
    <key>CFBundleExecutable</key>              <string>micbridge-gui</string>
    <key>CFBundleIconFile</key>                <string>micbridge</string>
    <key>CFBundlePackageType</key>             <string>APPL</string>
    <key>CFBundleShortVersionString</key>      <string>${VERSION}</string>
    <key>CFBundleVersion</key>                 <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>          <string>11.0</string>
    <key>NSHighResolutionCapable</key>         <true/>

    <!-- Shown verbatim in the permission dialog. Says what is captured and where
         it goes, because "micbridge would like to access the microphone" without
         a reason is what people deny. -->
    <key>NSMicrophoneUsageDescription</key>
    <string>micbridge captures this machine's audio input and streams it to another computer on your network, so an application there can use it as a microphone.</string>

    <!-- The receiver announces itself and answers probes on the local network. -->
    <key>NSLocalNetworkUsageDescription</key>
    <string>micbridge finds receivers on your local network so you do not have to type an address.</string>
</dict>
</plist>
PLIST

# Ad-hoc signature. Not a substitute for notarization — Gatekeeper still warns —
# but TCC identifies an app by its code signature, and an unsigned binary is a
# different app on every rebuild: the microphone permission would be asked for
# again and again and never stick.
codesign --force --deep --sign - "$APP" 2>/dev/null \
    || echo "warning: ad-hoc signing failed; the microphone permission may not persist" >&2

echo "built $APP"
du -sh "$APP" | awk '{print "  " $1}'
echo
echo "Run it:            open $APP"
# Not "right-click -> Open": macOS 15 removed that bypass, and on 15 and later it
# fails exactly like a double-click. Clearing the quarantine flag still works, and
# a locally built app has no such flag in the first place.
echo "First launch:      xattr -dr com.apple.quarantine $APP   (it is not notarized)"
echo "Or:                System Settings -> Privacy & Security -> Open Anyway"
