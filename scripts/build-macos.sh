#!/usr/bin/env bash
# Builds the native macOS binary.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release

echo
for bin in target/release/micbridge target/release/micbridge-gui; do
    echo "built $bin"
    ls -lh "$bin" | awk '{print "  " $5}'
done

echo
echo "GUI:  ./target/release/micbridge-gui"
echo "CLI:  ./target/release/micbridge devices"
