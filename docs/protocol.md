# The micbridge protocol (version 1)

One sender captures audio and one receiver renders it. There are two channels: a
TCP control channel that negotiates and reports, and a UDP media channel that
carries PCM one way and never looks back.

Any change to anything on this page changes `crates/micbridge-protocol` with it, and
vice versa — see `CONTRIBUTING.md`.

## Transport

* **Control — TCP, port 42100 by default.** Session setup, format negotiation,
  heartbeats, statistics. Reliable and ordered, because losing a handshake is
  not survivable.
* **Media — UDP, port 42101 by default.** Interleaved PCM. One-way, never
  retransmitted, loss-tolerant.

Both defaults sit clear of the GameStream range (47984–47990 and 48010) that
Sunshine and Moonlight already occupy, since the expected deployment has both
running between the same two machines.

The receiver binds its media socket **per session**, not once per process, so a
reconnecting sender cannot be fed leftover datagrams from the previous one. If
the preferred port is taken the receiver falls back to an ephemeral one and
reports the port it actually got in `hello_ack`, so nothing needs reconfiguring
on the sending side.

## Discovery — UDP, port 42102 by default

A sender that has not been told an address broadcasts a probe; receivers answer with
the control port to use. Optional in both directions: a receiver may refuse to
announce, and a sender given `--host` never probes.

Magic is **`0x5244`** (`"RD"`), deliberately *not* the media channel's `0x5250`. They
were the same at first and it collided: the version bytes match, and a media header's
`channel = 0` (audio) is byte-for-byte a probe's `kind = 0`, so every audio datagram
parsed as a valid probe. Different ports made that harmless in practice, but
"harmless in practice" is how a receiver ends up answering a stream of audio with
discovery replies.

| Message | Layout |
|---|---|
| Probe | magic u16, version u8, kind u8 = 0 — four bytes total |
| Announce | magic u16, version u8, kind u8 = 1, control_port u16, label_len u8, label bytes |

The label is at most 64 bytes of UTF-8 and may be empty. A decoder rejects a length
byte claiming more than it received rather than over-reading, and rejects a truncated
announce outright rather than believing a partial port.

A probe is repeated a few times inside its window, because a single unacknowledged
broadcast can simply vanish. Replies are **unicast** back to the prober, so one
curious host does not wake the segment.

The discovery version is bumped independently of `PROTOCOL_VERSION`: an old sender
should still be able to *find* a newer receiver, even if it then fails the session
handshake with a clear error.

### Discovery is never the only way in

Broadcast reaches one network segment. It does not cross Tailscale, a VPN, or a
routed subnet, and some Wi-Fi access points filter it. So a receiver also reports the
addresses a peer could reach it on, which works in all of those cases.

Those addresses are obtained without enumerating interfaces — which would need
platform-specific code on both targets — by `connect`ing a UDP socket toward a
representative destination and asking the socket which local address the kernel
chose. `connect` on UDP sends no packets, so this is instant and silent. One probe
toward a public address finds the default-route address; one toward `100.64.0.1`
finds the Tailscale address when that interface is up.

## Security

**Version 1 has no authentication and no encryption.** It is intended for a
wired LAN or a Tailscale network, and the README says so plainly.

The receiver drops media datagrams whose source address does not match the
control connection's peer. That is a hygiene filter, not a security control — a
spoofed source address defeats it, and it does nothing about an observer on the
path. Since the payload is an open microphone, treat the channel as public.

The upgrade path is a pairing pre-shared key plus ChaCha20-Poly1305 over the
media payload, with the header left in the clear so a receiver can still route a
datagram before authenticating it. This is a known gap, deliberately left rather
than overlooked.

## Framing on the control channel

Each message is one frame: a **4-byte big-endian length prefix** followed by that
many bytes of payload. The payload is MessagePack, encoded as maps with **named**
fields.

Named fields rather than positional arrays is what makes the format extensible:
adding a field is backward compatible in both directions, because an older peer
ignores a key it does not recognise and `serde`'s `default` fills one that is
absent. Only a change an existing peer would *misparse* needs the protocol
version bumped.

Maximum frame length is 1 MiB. Real control frames are a few hundred bytes; the
cap exists so a corrupt or hostile length prefix cannot make the receiver
allocate before it has read a single byte of payload.

The sender's first frame must be `hello`. A receiver that gets anything else
answers with `error` and closes.

### Reading a frame must be resumable

Both peers put a read timeout on the control socket so their own loop gets a turn
to send heartbeats. An implementation must therefore keep partial progress across
a timeout, because a timeout can land **between the length prefix and its
payload**.

Getting this wrong desynchronises the connection permanently rather than
transiently: a reader that discards a partial frame treats the next payload byte
as a length prefix, and reports either an absurd frame length or a decode error
for every frame thereafter.

It does not take a broken link. Windows' minimum TCP retransmit timeout is 300 ms
against a 200 ms poll, so a single dropped segment over Wi-Fi or Tailscale is
enough. The reference implementation uses a small state machine
(`framing::FrameReader`) holding the prefix bytes and payload bytes read so far,
and reports a timeout as "no frame yet" rather than as an error.

One reader belongs to one connection for its whole life. Constructing a fresh one
per read reintroduces exactly this bug.

### Handshake timeout

Each side gives the other five seconds to complete the handshake. Without a
bound, a peer that accepts the TCP connection and then says nothing — a half-open
NAT mapping, or a receiver wedged on a device that will not open — hangs the other
side indefinitely, indistinguishable from a slow link.

## Handshake

Sender → receiver:

```json
{"hello": {
  "protocol_version": 1,
  "session_id": 6831025583518260520,
  "device_name": "UMC204HD 192k",
  "format": {
    "sample_rate": 48000,
    "channels": 2,
    "sample_format": "s16_le",
    "frames_per_packet": 240
  }
}}
```

`device_name` is for the receiver's log only; the receiver never matches on it.

`session_id` distinguishes a reconnect from a second concurrent sender. It is not
a credential and does not need to be unpredictable, only unlikely to repeat. Note
that wall-clock nanoseconds alone are *not* sufficient: `SystemTime` on macOS is
not actually nanosecond-granular, so two senders started in the same instant read
the same value. The reference implementation mixes in a process-local counter.

Receiver → sender:

```json
{"hello_ack": {
  "protocol_version": 1,
  "media_port": 42101,
  "target_buffer_ms": 20,
  "format": { "...": "echoed unchanged" }
}}
```

**Version 1 does not renegotiate.** The receiver either accepts the sender's
format exactly or fails the handshake with `error`. Quietly substituting a
different rate or channel count would surface as pitch-shifted or half-silent
audio, which is far harder to diagnose than a refusal at startup.

The receiver rejects a format with zero channels, a zero sample rate, zero
frames per packet, or a datagram larger than its 8192-byte receive buffer.

A receiver must resolve its output device **before** sending `hello_ack`, and
reject the session if that fails. Everything local that can go wrong — a missing
device, an unwritable file, a channel count it cannot honour — is then reported
while the sender is still waiting for an answer, instead of a moment after it has
been told to start streaming. A session that is accepted and then dies leaves the
sender transmitting into nothing, with silence as its only clue.

Note that the wire format's channel count is the **sender's**, and a receiver is
not required to open its device at that count — see "Channel counts" below.

## Steady state

Both sides send `heartbeat` every second and `stats` alongside it. A peer that
has said nothing for four seconds is declared gone.

Four heartbeats is deliberately generous: tearing a session down costs a
re-prebuffer and throws away the accumulated clock-drift estimate, and a
momentarily busy machine is not a disconnect.

`stats` is observability only — neither side changes behaviour based on what the
other reports.

Sender: `frames_captured`, `packets_sent`, `frames_dropped`. Receiver:
`packets_received`, `frames_lost`, `packets_late`, `packets_reordered`,
`underruns`, `overruns`, `buffer_fill_ms`, `resample_ratio`.

The sender closes with `goodbye`. A dropped connection is equivalent; `goodbye`
only saves the receiver a four-second timeout.

## The media datagram

A fixed 16-byte big-endian header, then payload:

| Offset | Size | Field        |
|-------:|-----:|--------------|
|      0 |    2 | magic `0x5250` (`"RP"`) |
|      2 |    1 | version      |
|      3 |    1 | channel      |
|      4 |    4 | seq          |
|      8 |    8 | sample_idx   |

**There is no flags byte.** Extension goes through `channel` for a new payload
kind and `version` for an incompatible change to an existing one, which covers
what a flags byte would have done while keeping `sample_idx` at a natural 8-byte
offset.

`channel` is `0` for audio and `1` for HID. HID is **reserved and unimplemented**
— the constant exists so the byte is not quietly reused, and so a receiver that
meets one can name it in a log line while ignoring it.

`seq` increments per datagram and wraps. It is used only to count loss and spot
reordering, never for placement.

`sample_idx` is the index of the first frame in the payload, counted from the
start of the sender's capture stream. **It is a frame counter, not a clock.** That
is the single most important decision on this page:

* The two machines never have to agree on a time base, so there is no clock
  synchronisation anywhere in the protocol.
* Where a datagram belongs is independent of how long it spent in flight, so
  jitter cannot reorder audio.
* Drift is therefore measured from buffer occupancy instead, which is the one
  observable that actually reveals it.

A stream does not begin at index zero. The sender may have been capturing long
before the receiver connected, so the receiver takes its origin from the first
datagram it sees.

At 48 kHz a `u64` frame counter lasts about twelve million years, so wrap is not
a case the receiver handles.

Payload is interleaved little-endian `i16`. The default of 240 frames is 5 ms at
48 kHz, giving 960 payload bytes and a 976-byte datagram — comfortably inside any
path's MTU, which is why that is the default.

`i16` conversion scales by **32768, not 32767**, so a sample that originated as an
`i16` survives the round trip bit-exactly and full-scale negative does not clip.
Encoding clamps, because `+1.0` scales to 32768 — one past the top of the range —
and would otherwise wrap to full-scale negative as a loud click.

## What the receiver does with a datagram

1. Drop it unless the source address matches the control peer.
2. Parse and validate the header; drop it on bad magic or an unknown version.
3. Drop it unless `channel` is audio.
4. Drop it if the payload is not a whole number of frames. A truncated frame
   would shift the channel interleave for the remainder of the session, so a
   partial payload is discarded whole rather than partly accepted.
5. Place it by `sample_idx`:
   * Equal to the next expected index — append, then release anything that just
     became contiguous.
   * Ahead of it — hold it, up to the reorder window.
   * Behind it — its audio has already been played. Count it late and drop it. A
     datagram that arrives after its moment is worth less than the silence it
     would displace.
6. When the reorder window fills, give up on the missing datagram, substitute
   silence for it, and drain.

### The reorder window is bounded by the buffer

Holding datagrams back to wait for a missing one stalls the stream for as long as
the wait lasts, and that stall is paid for out of the jitter buffer. A window of
`w` packets means up to `w + 1` packet intervals with nothing entering the ring.

So a window sized at or above the buffer target **guarantees the underrun it was
meant to prevent**. This is not hypothetical: a 20 ms target with a 4-packet
window at 5 ms per packet underran on every single lost packet.

The implementation therefore derives the window from the target, budgeting half
of it and keeping the other half as margin for the jitter the buffer mainly
exists for. Below roughly three packets of target there is no room for tolerance
at all and the window falls to one, which still recovers a straight swap of two
adjacent datagrams. Wanting deeper reorder tolerance means raising the buffer
target and paying for it in latency.

## Channel counts

The `channels` field describes the **sender's** stream. It is not a demand on the
receiver's device.

A receiver opens its output at the device's own channel count and maps the stream
onto it. That is not a convenience: WASAPI in shared mode will not open a stream at
any other count. cpal never sets `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, so the
Windows audio engine performs no channel conversion and `IsFormatSupported`
rejects a mismatch outright. CoreAudio's AUHAL converts silently, which is why an
implementation that demands the wire count appears to work on macOS and fails on
every Windows endpoint whose mix format differs — a mono capture source against
VB-CABLE's stereo endpoint being the obvious case.

The reference mapping, all of it in `micbridge_core::channels`:

| Stream | Device | Behaviour |
|---|---|---|
| n | n | copied through |
| 1 | n | replicated to every output channel |
| n | 1 | averaged, not truncated, so a hard-panned signal does not vanish |
| n | m > n | copied in order, remaining channels silent |
| n | m < n | leading m channels copied, the rest discarded |

## Clock drift

The sender's capture clock and the receiver's render clock are different
crystals, typically tens of parts per million apart. Nothing in the protocol can
fix that — over an hour, 50 ppm is 180 ms of audio that has to come from
somewhere or go somewhere.

The receiver corrects for it by resampling at a continuously variable ratio
driven by a PI controller on buffer occupancy, so its consumption rate tracks the
sender's true production rate. The correction is clamped to ±5000 ppm, two orders
of magnitude more headroom than any real crystal pair needs; the clamp is there
so a pathological occupancy reading cannot command an audible rate jump.

None of this appears on the wire. It is named here because an implementation that
omits it appears to work for several minutes and then begins to click, and the
cause is not discoverable from the protocol alone.
