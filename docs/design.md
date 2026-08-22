# Design notes

Why this is shaped the way it is, including the approaches that were rejected and
the reasons they were rejected. The wire format lives in `protocol.md`.

## The problem

A Behringer UMC204HD is plugged into a Mac. Games run on a Windows machine and are
streamed to the Mac with Moonlight. A game on the Windows box has no way to hear
the interface.

Return audio — game sound arriving at the interface — already works through
Moonlight's own `hostaudio` path and is out of scope.

## Translate, don't tunnel

The obvious framing is "move the USB device to the PC". Two things rule it out.

**macOS will not release the device.** IOKit claims class-compliant audio and HID
devices at enumeration. Taking one away from its own driver requires a DriverKit
system extension holding `com.apple.developer.driverkit.transport.usb`, an
entitlement Apple grants case by case. This is why there is no `usbip` client for
macOS, and why VirtualHere ships its own signed driver to do it.

**USB audio does not survive a network.** USB Audio Class uses isochronous
transfers: a packet every 125 µs at high speed, with a hardware timing contract
and no retries. Tunnelling that over IP asks a network with millisecond-scale
jitter to honour a hardware clock. USB HID, by contrast, is interrupt-driven,
low-rate, and tunnels fine — which is why "HID over USB/IP works, audio crackles"
is the consistent experience with off-the-shelf tools.

So the device is never moved. It is read as an ordinary CoreAudio input, the PCM
is shipped over the network, and the receiver renders it into a virtual endpoint
that Windows presents as a recording device. The interface stays fully usable on
the Mac throughout, and the path works over Tailscale and any WAN, which USB/IP
does not.

The cost is that Windows sees a generic recording endpoint rather than a
Behringer, so the vendor's own Windows control panel and ASIO are not available
there. Given the goal is "a game can hear this input", that costs nothing:
games read microphones through WASAPI defaults and never ask what the hardware is.

## The virtual endpoint is bought, not built

Presenting audio as a *recording* device on Windows means a driver. Writing one
means a WDM/SysVAD derivative, an EV code-signing certificate, and WHQL
attestation — the item that would have dominated the schedule.

VB-CABLE already is that driver: signed, donationware, and installed by one
double-click. The receiver renders into "CABLE Input" and the game selects "CABLE
Output" as its microphone. Every line of code in this repository is user-mode as a
direct result.

Valve reached the same conclusion, which is visible on any Mac with Steam installed:
`/Library/Audio/Plug-Ins/HAL` contains `SteamStreamingMicrophone.driver` and
`SteamStreamingSpeakers.driver`, a virtual cable pair serving exactly this purpose
for Steam Remote Play.

## Why Rust, and one binary

Rust because every dependency needed is pure Rust. That is not incidental: it is
what makes `cargo xwin build --target x86_64-pc-windows-msvc` work from the Mac
with no C cross-toolchain. cpal reaches WASAPI through the `windows` crate, which
is generated bindings rather than a C library to link.

One binary with `send` and `recv` subcommands rather than two programs, because
cpal covers CoreAudio and WASAPI behind one API, so there is almost no
platform-specific code to separate. Keeping them together means the wire format
cannot drift between the two halves of a release, and it makes
`micbridge send | micbridge recv` on one machine a complete loopback test.

## One engine, two frontends

`micbridge-engine` holds everything about what a session *does*; `micbridge` and `micbridge-gui` only
turn a request into a config and display the result. Without that split there would
be two implementations of the session lifecycle, and the GUI's would be the one
nobody tested.

A session runs on its own thread because a `cpal::Stream` is not `Send` on macOS —
it has to be created, held and dropped on one thread, and that thread also runs the
control loop. Watchers only read `SessionState`, which is safe to poll from a paint
loop.

Two details that had to be got right:

* A session is `Starting` from the moment the handle exists, set on the caller's
  thread before the session thread is spawned. Leaving it to the session body left a
  window where the status read `Idle` — indistinguishable from "finished" — so
  anything gating a Stop button on "is it active" switched itself off immediately.
* The receiver's listener is non-blocking. A blocking `accept` ignores both the
  duration deadline and a stop request, which leaves the GUI's Stop button dead
  until a sender happens to connect.

## Why eframe, and the renderer it pins the GUI to

eframe because it is pure Rust, and pure Rust is the constraint that keeps the
Windows cross-build working without a C toolchain — the same constraint that ruled
out libopus.

Pinned to eframe 0.35 rather than 0.36 because 0.36 raised its MSRV to rustc 1.95,
which would mean a Homebrew-installed Rust could no longer build this repository at
all. 0.35 needs 1.92, so a Homebrew toolchain and a rustup one both work.

`default-features = false` with just `glow` and `default_fonts`, which is a
compromise rather than a preference. glow reaches OpenGL through pure-Rust bindings,
but **it fails to start over Windows Remote Desktop** — RDP exposes only Microsoft's
GDI generic OpenGL 1.1 driver, and egui does not degrade under that, it refuses
(egui issues #2573, #3165). For a tool whose entire purpose is a remote Windows
machine, that is a real limitation.

The obvious answer, enabling wgpu and falling back to Direct3D 12, was tried and
does not build: `wgpu-hal 29` requires `gpu-allocator ^0.28` and `windows ^0.62`,
while the only 0.28 release of gpu-allocator depends on `windows` 0.54. The D3D12
types do not unify and the backend fails to compile — natively as much as
cross-built, and no version pinning reconciles two mutually exclusive constraints.

So the GUI is OpenGL-only and the RDP case is answered by the CLI, which has no
renderer at all. Revisit when eframe's MSRV and a coherent wgpu/gpu-allocator pair
are both acceptable.

## Reading a frame has to be resumable

Both control loops put a read timeout on their socket so they get a turn to send
heartbeats. That makes `read_exact` unusable: it consumes bytes into its buffer and
*then* returns the timeout error, so whatever was already taken off the socket is
lost. One timeout landing between a length prefix and its payload desynchronises the
connection permanently — every subsequent frame is garbage, reported as either an
absurd frame length or a decode error.

It does not take a broken link: Windows' minimum TCP retransmit timeout is 300 ms
against a 200 ms poll, so one dropped segment over Wi-Fi or Tailscale is enough.

`framing::FrameReader` is therefore a small state machine that keeps partial
progress and reports a timeout as "no frame yet". One reader lives as long as its
connection; a fresh one per read would reintroduce the bug it exists to prevent.

## The device's format is authoritative, both dimensions of it

The receiver already took the device's *sample rate* as given and let the resampler
bridge the difference. It did not do the same for the channel count, and instead
asked the device for the stream's — which is fine on CoreAudio, where AUHAL converts
silently, and fatal on WASAPI, which in shared mode supports only the endpoint's own
count and rejects anything else outright.

Worse, it failed *after* the handshake was acknowledged, so the sender was told to
start streaming into a session that then died.

Both halves are fixed: the device is resolved before the ack, and
`micbridge_core::channels::ChannelMap` adapts the stream onto whatever the device wants —
mono fanned out, extra channels padded with silence, a downmix averaged rather than
truncated so a hard-panned signal does not vanish. The mapping is logged, because a
silent fan-out and a silent channel drop both sound plausible until someone wonders
why.

## The level meter is not decoration

"Is audio actually flowing" is the first question anyone asks when bringing this up,
and no counter answers it — a packet counter climbs just as happily when the input
is muted. So there is one `LevelMeter`, lock-free, written from whichever audio
callback is live and read by the frontend.

It is measured on what was handed to the *device*, after resampling, not on what
arrived from the network. A meter fed from the network keeps moving during an
underrun, when the device is in fact receiving silence.

Displayed on a dB scale with a slow fall-off. A linear bar spends nearly all its
travel in the top few dB and shows nothing for quiet speech; and a bar drawn
straight from peak-since-last-poll flickers unreadably, which is why every hardware
meter rises instantly and falls slowly.

## Dependencies deliberately not taken

**No tokio.** Two sockets and a realtime audio callback. The callback may not
allocate, lock, or block, so it cannot touch an async runtime; and with two
sockets a runtime buys nothing. Blocking `std::net` on dedicated threads instead.

**No Opus and no libsamplerate**, though both are a `brew install` away. On a
wired LAN, 48 kHz stereo `i16` is 1.5 Mbit/s against a link already carrying
Moonlight at 150 Mbit/s — a codec would save nothing measurable while adding
roughly 6.5 ms of algorithmic lookahead to a 20 ms budget, and it would make a
capture path lossy. A C dependency would also break the cross-compile above.

Opus becomes the right answer the moment this leaves a wired LAN, and
`WireSampleFormat` is a negotiated enum so it can arrive as a variant. A receiver
meeting an unknown variant fails the handshake rather than rendering noise.

## The resampler is hand-written

Drift correction needs a ratio adjustable by parts per million on every callback,
which rules out the usual fixed-ratio designs and makes the streaming glue around
a chunked library resampler awkward. What is left is a fractional read position
with interpolation — which *is* the resampler, in about sixty lines.

Four-point cubic Hermite with Catmull-Rom tangents. At ratios near 1.0, where
drift correction lives, its error is far below the capture device's noise floor;
verified end to end, a 1 kHz tone comes out at exactly the right amplitude with a
crest factor of 1.41 and 5000× less energy in the neighbouring bins.

It is *not* a high-quality rate converter. For a large fixed conversion such as
44.1 to 48 kHz it is adequate for voice and would not satisfy a mastering
engineer. Setting the interface to 48 kHz avoids the question, which is why the
README recommends it. A band-limited sinc converter is the upgrade path if the
large-ratio case ever matters.

## Thread layout

```
Sender                                    Receiver
──────                                    ────────
CoreAudio callback ─┐                     ┌─ media thread
  copy to ring      │                     │    parse, sequence, place
                    │                     │
              lock-free                   │  lock-free ring
                ring                      │
                    │                     │
media thread ───────┘        UDP ─────────┘  WASAPI callback
  packetise, i16, send                       drift controller
                                             resampler
control: main thread                       control: main thread
```

Everything that can allocate, hold a `BTreeMap`, or take an unbounded amount of
time is on a network thread. Both audio callbacks do arithmetic over
pre-allocated buffers and nothing else.

The rings are lock-free SPSC rather than `Mutex<VecDeque<_>>`. A `try_lock` that
fails only occasionally still turns into an audible click every few minutes, which
is precisely the class of bug this project exists to avoid.

Every ring operation moves **whole frames**. A partial frame would shift the
channel interleave permanently, so the consumer checks occupancy before reading
rather than trusting a slice-based pop.

## Pre-buffering, and why it is not an underrun

Playback waits for the buffer to reach its target before starting. Without it the
first seconds are a fight: the controller sees an empty buffer, commands its
maximum slow-down, and takes several seconds to recover. Pre-buffering starts the
loop already at its setpoint.

Silence written during that phase is expected and is deliberately *not* counted as
an underrun, so the soak test's "zero underruns" assertion means what it says.

## What is not here

* **HID.** Out of scope for this version. The media channel reserves a channel
  byte for it so the number is not reused, and nothing more.
* **Multi-channel beyond what the device offers.** Stereo in, stereo out.
* **Encryption.** See `protocol.md`.
* **Renegotiation.** The receiver accepts the sender's format or refuses.
* **More than one concurrent sender.** The receiver serves one session at a time.
