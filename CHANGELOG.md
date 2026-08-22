# Changelog

Notable changes, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning treats **the wire format as the public interface**: the frames in
`docs/protocol.md` are what a second implementation would be written against, so a
change that breaks them is a major version, whatever it does to the command line.

## [1.0.1] — 2026-08-22

### Fixed

* **The light theme is no longer black.** eframe's default `clear_color` ignores
  the visuals it is handed and clears the window to a hardcoded
  `rgba(12, 12, 12, 180)`. Nothing else paints the window background — `App::ui`
  hands over a `Ui` that has none — so that colour *was* the background. It sits a
  few points from the dark palette's `#0F1518`, which is why the dark theme looked
  right and hid the bug; under the light palette it framed white cards in black and
  left the wordmark and every section label unreadable. The window now clears to
  `panel_fill`, which follows the system theme.

### Changed

* The macOS install instructions no longer say "right-click → Open". Apple removed
  that Gatekeeper bypass in macOS 15 Sequoia, so on Sequoia and Tahoe the advice
  sent people in a circle. The README and `scripts/bundle-macos.sh` now give the
  `xattr` command and the *Privacy & Security → Open Anyway* route, and are
  straight about the app being ad-hoc signed rather than unsigned.

## [1.0.0] — 2026-08-22

First release.

* **Capture on one machine, a microphone on another.** `micbridge send` reads an
  audio input and ships PCM over the network; `micbridge recv` renders it into a
  playback endpoint that the other machine already presents as a recording device
  — VB-CABLE on Windows, a PipeWire null sink on Linux.
* **One binary per platform, plus a window.** `micbridge` (CLI) and
  `micbridge-gui` share `micbridge-engine`, so the session lifecycle has one
  implementation rather than two.
* **A level meter that answers "is audio actually flowing"**, on a dB scale with a
  latching clip badge, measured after resampling on what reached the device.
* **Jitter buffer with drift correction.** A hand-written cubic-Hermite resampler
  trimmed by parts per million, so two crystals that disagree do not accumulate
  into a click every few minutes. Pre-buffering is not counted as an underrun.
* **Channel and rate adaptation** onto whatever the receiving device demands,
  resolved before the handshake is acknowledged rather than after.
* **Discovery** over UDP broadcast on every interface, with the reachable
  addresses printed for the Tailscale and routed cases it cannot reach.
* **A menu bar item on macOS and a tray icon on Windows**, and an autostart entry
  on Windows that comes up receiving.
* Apache-2.0. The GUI embeds the Ubuntu and Noto Emoji fonts under their own
  licences — see `THIRD-PARTY.md`.

Not yet verified at this tag: the Linux binary has not been run, there has been no
thirty-minute soak, and latency is estimated rather than measured. The README's
*Unverified* section is the current list.
