# Testing

Three layers: automated tests that need no hardware, a local loopback that needs
only the Mac, and the cross-machine checks that need the Windows box.

## Automated

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features
cargo test --workspace --all-features
```

Run them locally: there is no CI on push. The release workflow runs the same
suite on macOS, Windows and Linux, but only for a tag or a manual dispatch.

Nothing in the suite touches an audio device, so it passes on a machine with no
sound hardware at all — which is what the WAV sink and the tone source exist
for.

The tests worth knowing about, because they encode findings rather than
restating the code:

* `drift::tests::uncorrected_drift_drains_the_buffer_dry` — 200 ppm over five
  minutes empties the buffer completely. This is the test that justifies the
  existence of `drift.rs`.
* `drift::tests::correction_holds_the_buffer_at_target` — with correction on, the
  same 200 ppm settles at target and the controller's trim converges on the true
  clock offset to within 20 ppm.
* `drift::tests::integral_recovers_promptly_after_being_pinned` — five minutes of
  empty buffer must not wind the integrator up into an overshoot on recovery.
* `pipeline::tests::the_reorder_window_always_fits_inside_the_buffer_target` —
  pins the invariant that a reorder window at or above the buffer target
  guarantees an underrun. A 4-packet window against a 20 ms target underran on
  every lost packet before this was derived from the target instead of guessed.
* `resample::tests::a_linear_ramp_is_interpolated_exactly_at_any_ratio` — cubic
  interpolation is exact for polynomials up to degree three, so a ramp pins the
  read position with no tolerance for a phase error.
* `wav::tests::end_to_end_pipeline_into_a_file_preserves_the_tone` — datagrams in
  one end, recognisable audio out of the other, with no hardware.

A note on `resample::tests::ratio_controls_how_fast_input_is_consumed`: it runs
10 000 output frames rather than 100. Producing N frames advances the position
N-1 times, so at ratio 1.01 over 100 frames the total advance is 99.99 —
indistinguishable from ratio 1.0. A test too short to resolve one percent would
not catch a sign error either.

## Local loopback on the Mac

No second machine, no VB-CABLE. Start the receiver writing to a file:

```sh
cargo build --release
./target/release/micbridge recv --sink wav --wav-path captures/loopback.wav --once --duration-secs 12
```

In another terminal, send a known signal:

```sh
./target/release/micbridge send --host 127.0.0.1 --tone 1000 --duration-secs 6
```

Expect zero underruns, zero overruns, zero loss, and `fill_ms` sitting within a
millisecond or so of the target. Then verify the audio rather than trusting the
counters:

```sh
python3 - <<'EOF'
import wave, struct, math
w = wave.open("captures/loopback.wav"); rate = w.getframerate(); ch = w.getnchannels()
s = struct.unpack("<%dh" % (w.getnframes()*ch), w.readframes(w.getnframes()))
seg = s[0::ch][rate*2:rate*3]          # one second, well past startup
peak = max(abs(x) for x in seg)
rms  = math.sqrt(sum(x*x for x in seg)/len(seg))
zc   = sum(1 for a, b in zip(seg, seg[1:]) if (a < 0) != (b < 0))
print("peak %d (expect 8192)  crest %.2f (expect 1.41)  freq %.1f Hz" % (peak, peak/rms, zc/2))
EOF
```

A verified run of the above: peak exactly 8192, crest 1.41, 1000.0 Hz, with
energy at 1 kHz some 5000× the neighbouring bins.

Swap `--tone 1000` for `--device UMC204HD` to check real capture. With nothing
plugged into the interface expect a peak in the tens — that is its noise floor,
and it confirms the microphone permission was granted. Silence that is exactly
zero means macOS denied it: grant the terminal microphone access in System
Settings → Privacy & Security.

## Mac to Windows

Set the UMC204HD to **48 kHz** in Audio MIDI Setup first. It defaults to 44.1 kHz,
which makes the receiver interpolate every sample; 48 kHz puts the nominal ratio
at exactly 1.0.

On the Windows box, install VB-CABLE, then:

```sh
micbridge.exe recv --device "CABLE Input" --target-buffer-ms 20
```

On the Mac, prove the network path before involving the microphone:

```sh
./target/release/micbridge send --host 192.168.1.20 --tone 1000
```

Then check, in order:

1. Windows Sound → Recording → **CABLE Output** shows a live level meter.
2. Windows Voice Recorder, input set to CABLE Output, captures the tone.
3. Re-run with `--device UMC204HD` and confirm the meter still moves.
4. Launch a game through Moonlight, select CABLE Output as its microphone.

If step 1 fails the problem is the network or the routing; if steps 1–2 pass and
3 fails, it is the capture side. That split is the whole reason `--tone` exists.

Over Tailscale, substitute the Tailscale address (`100.64.0.5`) for the LAN
one. Expect more jitter and consider `--target-buffer-ms 40`.

## Soak

Thirty minutes minimum. Shorter runs do not catch a wrong drift controller —
that is the failure mode that looks fine for twenty minutes and then starts
clicking.

```sh
# On the Windows box
micbridge.exe recv --device "CABLE Input" --duration-secs 1860 --once

# On the Mac
./target/release/micbridge send --host 192.168.1.20 --device UMC204HD --duration-secs 1800
```

Pass condition, from the `session summary` line: **`underruns=0` and
`overruns=0`**. `lost` and `late` should also be zero on wired Ethernet; non-zero
there points at the network, not at this program.

Watch `fill_ms` across the run. It should hold within a millisecond or two of the
target. A slow one-way walk means drift correction is not keeping up; `trim_ppm`
parked at ±5000 means it has hit its clamp and something other than crystal drift
is wrong.

## Latency

Loop a UMC204HD output back into its own input with a short cable. Play a click on
the Mac, capture CABLE Output on Windows, and measure the offset between the two
in Audacity.

Expected: roughly 35–55 ms one way with a 20 ms buffer, comprising capture
callback, packetisation, network, jitter buffer, render callback, and the game's
own capture buffer.

Record the number actually measured here when the hardware is available:

| Date | Buffer | Network | Measured one-way |
|------|--------|---------|------------------|
| _not yet measured_ | | | |
