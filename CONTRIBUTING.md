# Contributing

## Before opening a PR

```sh
cargo fmt --all
cargo clippy --all-targets --all-features
cargo test --workspace --all-features
```

**There is no CI, so running these locally is not a formality — it is the only
check that happens before review.** The release workflow compiles and tests on
macOS, Windows and Linux, but only when a tag is pushed, which is far too late to
learn that a change does not build.

That matters more here than in most projects, because the two halves of this
program run on different platforms and some differences show up on only one of
them: a read timeout surfaces as `WouldBlock` on Unix and `TimedOut` on Windows.
If a change touches anything platform-specific and you can only build on one
platform, say so in the PR rather than assuming it is fine.

## Looking at the window

The GUI can photograph itself, which is how it is checked:

```sh
cargo run -p micbridge-gui --features screenshot -- --screenshot shot.png
```

It renders through egui rather than the window server, so it needs no Screen
Recording permission and works on a machine with no display attached. To catch a
running session rather than an idle one, give it longer and start a sender in the
meantime:

```sh
MICBRIDGE_SCREENSHOT_FRAMES=260 cargo run -p micbridge-gui --features screenshot \
    -- --auto-receive --screenshot live.png &
sleep 3 && micbridge send --host 127.0.0.1 --tone 1000 --duration-secs 12
```

The writer emits stored deflate blocks so it needs no compression dependency,
which makes its output valid and about twenty times larger than necessary. A
screenshot headed for the repository goes through `scripts/optimize-png.py`
first — it takes a 1.7 MB capture down to about 50 KB.

A capture is also redacted on its way out: with `--screenshot` set, the window
renders the documentation addresses (`192.168.1.20`, `100.64.0.5`) instead of the
machine's real ones, and the device lists are filtered down to the hardware the
README names. Real addresses and every attached microphone are personal data, and
a screenshot in this repository is published; without the substitution the next
re-shoot quietly puts a home LAN and a phone's name back into the README. The
substitution lives in `crates/micbridge-gui/src/screenshot.rs` — an ordinary
`--features screenshot` run without `--screenshot` still shows real state.

The feature is off by default and nothing it adds is compiled into a release
build. Use it: this layout was first written without ever being looked at, and
every row was clipped against the window edge.

## Commits

Imperative subject lines: `add`, `fix`, `move`. Keep commits focused. A PR body
describes the user-visible effect.

## Any wire-format change updates `docs/protocol.md`

Non-negotiable, in both directions: a change to `crates/micbridge-protocol` updates the
document, and a change to the document lands with the code. The document is the
specification — it is what a second implementation would be written against — and
a stale one is worse than none.

`media::tests::wire_layout_is_pinned` snapshots the header bytes. If that snapshot
changes, the protocol changed, and the version constant and the document both need
looking at.

## Where things go

`micbridge-core` has no I/O dependency and must keep it that way. The jitter buffer, the
drift controller and the resampler are the parts most likely to be subtly wrong,
and keeping them pure is what lets them be tested against a simulated clock in
milliseconds instead of against real hardware over half an hour.

If you find yourself wanting a socket or a device handle in `micbridge-core`, the logic
probably belongs in `micbridge` or `micbridge-audio`.

## Realtime constraints

Two functions run in audio callbacks: `PlaybackSource::fill` and the capture
closure in `micbridge-audio::capture`. In those paths, and anything they call:

* No allocation. No `Vec` growth, no `format!`, no `tracing` macros.
* No locks, including `try_lock`. A lock that fails one time in ten thousand is an
  audible click every few minutes.
* No syscalls, no file or socket I/O.

Counters in these paths are atomics, and the reporting happens elsewhere.

## Tests should record findings, not restate the code

The valuable tests in this repository are the ones that pin down something that was
actually got wrong: that a reorder window at or above the buffer target guarantees
an underrun, that `SystemTime` on macOS is not nanosecond-granular, that a 100-frame
run cannot resolve a one percent ratio change. Each of those has a comment saying
what the failure looked like.

A test that asserts a getter returns what the setter set is not worth its
maintenance. A test whose comment explains why the obvious implementation was
wrong is worth a great deal.

## Adding a codec

There is a place for it: `WireSampleFormat` is a negotiated enum, so a new variant
is backward compatible and a receiver meeting an unknown one fails the handshake
rather than rendering noise.

Two constraints. Keep the dependency pure Rust or the Windows cross-build from
macOS stops working, which is a stated design goal. And measure the added latency
before and after — a codec that costs 6 ms against a 20 ms budget needs to be
earning it.
