# Changelog

Notable changes, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning treats **the wire format as the public interface**: the frames in
`docs/protocol.md` are what a second implementation would be written against, so a
change that breaks them is a major version, whatever it does to the command line.

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
