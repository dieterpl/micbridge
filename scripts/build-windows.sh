#!/usr/bin/env bash
# Cross-builds the Windows binary from macOS.
#
# This works only because every dependency is pure Rust: cpal reaches WASAPI
# through the `windows` crate, which is generated bindings rather than a C library
# to link, so no C cross-toolchain is needed. Adding a C dependency (libopus,
# libsamplerate) would break this — see docs/design.md.
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-msvc

if ! command -v rustup >/dev/null 2>&1; then
    cat >&2 <<'EOF'
error: rustup is not installed.

A Homebrew-installed Rust ships the host target only, and ignores
rust-toolchain.toml, so `rustup target add` is not available to it. Either:

  1. Install rustup from https://rustup.rs and re-run this script, or
  2. Let CI build the Windows binary — .github/workflows/ci.yml does it
     natively on windows-latest for every push.
EOF
    exit 1
fi

if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "adding target $TARGET"
    rustup target add "$TARGET"
fi

if ! command -v cargo-xwin >/dev/null 2>&1; then
    echo "installing cargo-xwin"
    cargo install cargo-xwin
fi

# xwin fetches the MSVC CRT and Windows SDK headers on first run; Microsoft's
# licence is accepted once and cached.
cargo xwin build --release --target "$TARGET"

echo
for bin in "target/$TARGET/release/micbridge.exe" "target/$TARGET/release/micbridge-gui.exe"; do
    if [ ! -f "$bin" ]; then
        echo "error: expected $bin" >&2
        exit 1
    fi
    echo "built $bin"
    ls -lh "$bin" | awk '{print "  " $5}'
done

echo
echo "Copy both to the Windows machine. Then either:"
echo "  micbridge-gui.exe                                (pick Receive, choose CABLE Input)"
echo "  micbridge.exe recv --device \"CABLE Input\""
