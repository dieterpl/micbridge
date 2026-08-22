#!/usr/bin/env bash
# Builds the native Linux binaries.
#
# Unlike the Windows cross-build, this needs system C libraries: cpal talks to
# ALSA, and eframe's windowing backends need X11 and OpenGL headers. That is why
# there is no cross-build script for Linux from macOS — see docs/design.md.
set -euo pipefail

cd "$(dirname "$0")/.."

missing=()
for lib in alsa x11 xkbcommon gl; do
    pkg-config --exists "$lib" 2>/dev/null || missing+=("$lib")
done

if [ ${#missing[@]} -gt 0 ]; then
    cat >&2 <<EOF
error: missing development packages for: ${missing[*]}

Debian / Ubuntu:
  sudo apt-get install -y libasound2-dev libx11-dev libxkbcommon-dev \\
      libgl1-mesa-dev libxcursor-dev libxi-dev libxrandr-dev

Fedora:
  sudo dnf install alsa-lib-devel libX11-devel libxkbcommon-devel \\
      mesa-libGL-devel libXcursor-devel libXi-devel libXrandr-devel
EOF
    exit 1
fi

cargo build --release

echo
for bin in target/release/micbridge target/release/micbridge-gui; do
    echo "built $bin"
    ls -lh "$bin" | awk '{print "  " $5}'
done

cat <<'EOF'

To receive into a virtual microphone, create one first — Linux has no
VB-CABLE, but PipeWire and PulseAudio can make the same pair of endpoints:

  pactl load-module module-null-sink \
      sink_name=MicBridge_Input \
      sink_properties=device.description=MicBridge_Input

  pactl load-module module-remap-source \
      master=MicBridge_Input.monitor \
      source_name=MicBridge_Output \
      source_properties=device.description=MicBridge_Output

The Input/Output naming is not decoration: micbridge pairs the two halves of a
cable by that convention, so named this way the receiver finds the route
itself. Check with `micbridge devices`, then select MicBridge_Output as the
microphone in the application that should hear the audio.
EOF
