//! The receiving end: accept a stream and render it locally.
//!
//! On Windows the output device is VB-CABLE's input, which makes the audio appear
//! as "CABLE Output" — a recording device any game or voice client can select as
//! its microphone. That indirection is the whole reason no driver had to be written
//! for this project.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use micbridge_audio::render::{self, RenderConfig};
use micbridge_core::pcm;
use micbridge_core::pipeline::{self, MediaSink, PipelineConfig, PipelineStats};
use micbridge_protocol::{
    framing, ClientMessage, FrameReader, HelloAck, ServerMessage, StreamFormat, CHANNEL_AUDIO,
    CHANNEL_HID, PROTOCOL_VERSION,
};

use crate::config::{ReceiverConfig, Sink};
use crate::state::{SessionState, Status};
use crate::timing::{
    self, is_timeout, ACCEPT_POLL, CONTROL_POLL, HANDSHAKE_TIMEOUT, HEARTBEAT, PEER_TIMEOUT,
};

/// Datagram receive buffer. Larger than any datagram this protocol produces, so a
/// truncating read is never mistaken for a short packet.
const MAX_DATAGRAM: usize = 8192;

/// Runs a receive session, serving senders until stopped.
pub fn run(config: &ReceiverConfig, state: &Arc<SessionState>) -> Result<()> {
    // Set once, here, rather than per session: a receiver in --once=false mode
    // serves one sender after another, and a gain the user set should outlive the
    // sender that happened to be connected when they set it.
    state.gain().set_db(config.gain_db);

    let listener = TcpListener::bind((config.bind.as_str(), config.port))
        .with_context(|| format!("binding control port {}:{}", config.bind, config.port))?;
    let local = listener.local_addr().context("reading local address")?;

    tracing::info!(control = %local, target_buffer_ms = config.target_buffer_ms, "waiting for a sender");

    // The bind address is normally 0.0.0.0, which is true and useless: what the user
    // needs is the address to type on the *other* machine. Report the addresses a
    // peer could actually reach, which also covers Tailscale, where broadcast
    // discovery does not reach at all.
    let reachable = crate::discovery::local_addresses();
    for ip in &reachable {
        tracing::info!(address = %format!("{ip}:{}", local.port()), "reachable here");
    }
    if reachable.is_empty() {
        tracing::warn!("no non-loopback address found; this machine may be offline");
    }
    let reachable_strings: Vec<String> =
        reachable.iter().map(|ip| format!("{ip}:{}", local.port())).collect();

    state.update_stats(|s| {
        s.local_addr = local.to_string();
        s.reachable = reachable_strings.clone();
    });
    if let Some(first) = reachable_strings.first() {
        state.push_log(format!("send to {first}"));
    }
    state.set_status(Status::Starting(format!("listening on {local}")));

    // Answer discovery probes for as long as the receiver runs, so a sender can find
    // this machine without being told an address. Best-effort on purpose: another
    // process may already hold the discovery port, and that is no reason to refuse to
    // serve audio.
    let _responder = if config.announce {
        let label = if config.label.is_empty() {
            format!("receiver on {}", local.port())
        } else {
            config.label.clone()
        };
        match crate::discovery::respond(
            local.port(),
            label,
            config.discovery_port,
            state.stop_flag(),
        ) {
            Ok(responder) => {
                tracing::info!(port = responder.port, "answering discovery probes");
                Some(responder)
            }
            Err(err) => {
                tracing::warn!("discovery unavailable, senders must be given an address: {err:#}");
                state.push_log(format!("discovery unavailable: {err:#}"));
                None
            }
        }
    } else {
        None
    };

    // Non-blocking so a stop request or a duration deadline is honoured while
    // nothing is connected. A blocking `accept` would hang until a sender turned
    // up, which leaves the GUI's Stop button dead and the soak harness unable to
    // bound the process.
    listener.set_nonblocking(true).context("setting the listener non-blocking")?;

    let started = Instant::now();
    let deadline = config.duration_secs.map(|s| started + Duration::from_secs(s));

    loop {
        let (control, peer) = match wait_for_sender(&listener, deadline, state)? {
            Some(accepted) => accepted,
            None => {
                tracing::info!("stopped before a sender connected");
                return Ok(());
            }
        };
        tracing::info!(%peer, "sender connected");
        state.push_log(format!("sender connected from {peer}"));

        // A socket accepted from a non-blocking listener inherits non-blocking on
        // Unix, which would turn every control read into a spurious `WouldBlock`.
        control.set_nonblocking(false).context("returning the control socket to blocking")?;

        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|r| r.is_zero()) {
            return Ok(());
        }

        match serve_session(control, peer, config, state, remaining) {
            Ok(()) => {
                tracing::info!("session ended");
                state.push_log("session ended".to_string());
            }
            // A failed session is not a failed process: the sender may simply have
            // been killed, and the next one should still be served.
            Err(err) => {
                tracing::warn!("session failed: {err:#}");
                state.push_log(format!("session failed: {err:#}"));
            }
        }

        if config.once || state.stop_requested() {
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
        state.set_status(Status::Starting(format!("listening on {local}")));
    }
}

/// Polls for an incoming connection until stopped or past `deadline`.
fn wait_for_sender(
    listener: &TcpListener,
    deadline: Option<Instant>,
    state: &Arc<SessionState>,
) -> Result<Option<(TcpStream, SocketAddr)>> {
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(Some(accepted)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if state.stop_requested() || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Ok(None);
                }
                std::thread::sleep(ACCEPT_POLL);
            }
            // A peer that connects and resets before the accept completes must not
            // take the receiver down with it. On Windows that surfaces here as
            // WSAECONNRESET, so a LAN port scan, a monitoring probe, or a sender
            // killed between its connect and our accept would otherwise exit the
            // process — on a machine where this is meant to sit waiting all day.
            // These are all "that connection is gone", never "this listener is
            // broken".
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::Interrupted
                ) =>
            {
                tracing::debug!(%err, "discarding a connection that died before accept");
            }
            Err(err) => return Err(err).context("accepting a control connection"),
        }
    }
}

fn serve_session(
    mut control: TcpStream,
    peer: SocketAddr,
    config: &ReceiverConfig,
    state: &Arc<SessionState>,
    remaining: Option<Duration>,
) -> Result<()> {
    control.set_nodelay(true).context("setting TCP_NODELAY")?;
    control.set_read_timeout(Some(CONTROL_POLL)).context("setting control read timeout")?;

    // One `FrameReader` for the life of the connection: it carries partial frames
    // across read timeouts, where a fresh reader per call would lose the bytes
    // already taken off the socket and desynchronise the stream for good.
    let mut reader = FrameReader::new();
    let hello = match reader
        .recv_until::<_, ClientMessage>(&mut control, Instant::now() + HANDSHAKE_TIMEOUT)
        .context("reading hello")?
    {
        ClientMessage::Hello(hello) => hello,
        other => {
            let message = format!("expected hello, got {other:?}");
            let _ = framing::send(&mut control, &ServerMessage::Error { message: message.clone() });
            bail!(message);
        }
    };

    if let Err(message) = validate(&hello.format, hello.protocol_version) {
        let _ = framing::send(&mut control, &ServerMessage::Error { message: message.clone() });
        bail!(message);
    }

    // Bind the media socket per session rather than once for the process, so a
    // reconnect cannot be fed stale datagrams from the previous one. If the
    // preferred port is taken, fall back to an ephemeral one — the sender learns
    // the real port from the handshake, so nothing has to be reconfigured.
    let media = bind_media(&config.bind, config.media_port)?;
    let media_port = media.local_addr().context("reading media port")?.port();

    let format = hello.format;
    tracing::info!(
        device = %hello.device_name,
        rate = format.sample_rate,
        channels = format.channels,
        packet_frames = format.frames_per_packet,
        packet_ms = %format!("{:.2}", format.packet_ms()),
        session = hello.session_id,
        media_port,
        "session negotiated"
    );

    // Resolve the sink completely *before* acknowledging the handshake. Everything
    // that can fail because of the local device or the filesystem then fails while
    // the sender is still waiting for an answer, instead of a moment after it has
    // been told to start streaming. Doing it the other way round meant a bad output
    // device produced a session that was accepted and then immediately died, and the
    // sender's only clue was silence.
    let mut target = None;
    let mut wav_sink = None;
    let (output_rate, output_channels, output_name) = match &config.sink {
        Sink::Device(_) => {
            let resolved = render::open(&render_config(config))?;
            let described = (resolved.sample_rate, resolved.channels, resolved.name.clone());
            target = Some(resolved);
            described
        }
        Sink::Wav(path) => {
            // Created here rather than on the writer thread so a locked or
            // unwritable file is an immediate handshake failure. On Windows, holding
            // the file open in a player is enough to cause it, and the previous
            // arrangement reported it only when the session ended — after minutes of
            // a receiver that looked healthy while writing nothing.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
            }
            wav_sink = Some(micbridge_audio::wav::WavSink::create(
                path,
                format.sample_rate,
                format.channels,
            )?);
            (format.sample_rate, format.channels, path.display().to_string())
        }
    };

    if output_rate != format.sample_rate {
        tracing::warn!(
            capture_rate = format.sample_rate,
            output_rate,
            "rates differ, so every sample is interpolated; setting both to the same rate is better for quality and CPU"
        );
        state.push_log(format!(
            "rate mismatch: capturing at {} Hz, rendering at {output_rate} Hz",
            format.sample_rate
        ));
    }

    // Work out which recording device a game should select, and say so. Nothing in
    // the audio stack will tell the user that rendering into "CABLE Input" means
    // listening to "CABLE Output", and getting it wrong produces silence from a
    // setup that is otherwise working perfectly.
    let game_device = match &config.sink {
        Sink::Device(_) => micbridge_audio::virtual_device::detect()
            .ok()
            .and_then(|routes| {
                micbridge_audio::virtual_device::route_for_render_device(&output_name, &routes)
            })
            .map(|route| {
                tracing::info!(
                    render_into = %route.render_into,
                    game_mic = %route.game_mic,
                    pairing = route.how.describe(),
                    "this output feeds a microphone"
                );
                state.push_log(format!(
                    "select {:?} as the microphone in your game",
                    route.game_mic
                ));
                route.game_mic
            })
            .unwrap_or_else(|| {
                // No microphone is fed by this device, so the audio is going to
                // wherever it physically goes — speakers, most likely — and no game
                // will ever be able to hear it. Worth saying, loudly.
                tracing::warn!(
                    device = %output_name,
                    "this output does not feed any microphone; a game cannot hear it"
                );
                state.push_log(format!(
                    "{output_name:?} feeds no microphone — a game cannot hear this"
                ));
                String::new()
            }),
        Sink::Wav(_) => String::new(),
    };

    state.update_stats(|s| {
        s.sample_rate = format.sample_rate;
        s.channels = format.channels;
        s.endpoint = output_name.clone();
        s.game_device = game_device.clone();
    });

    let mut pipeline_config = PipelineConfig::new(format, output_rate, config.target_buffer_ms);
    pipeline_config.output_channels = output_channels;
    let (sink, source, stats) = pipeline::build(pipeline_config);

    if !matches!(source.mapping(), micbridge_core::Mapping::Passthrough) {
        // Worth saying out loud: a silent mono-to-stereo fan-out and a silent
        // drop-the-rear-channels both sound plausible until someone wonders why.
        tracing::info!(
            stream_channels = format.channels,
            device_channels = output_channels,
            mapping = source.mapping().describe(),
            "channel counts differ"
        );
        state.push_log(format!(
            "{} channels in, {} out: {}",
            format.channels,
            output_channels,
            source.mapping().describe()
        ));
    }

    framing::send(
        &mut control,
        &ServerMessage::HelloAck(HelloAck {
            protocol_version: PROTOCOL_VERSION,
            media_port,
            target_buffer_ms: config.target_buffer_ms,
            format,
        }),
    )
    .context("sending hello ack")?;

    // A per-session flag, separate from the session-wide one, so a single failed
    // session tears down its own threads without ending the whole receiver.
    let session_stop = Arc::new(AtomicBool::new(false));
    let foreign = Arc::new(AtomicU64::new(0));

    let media_thread = {
        let stop = Arc::clone(&session_stop);
        let foreign = Arc::clone(&foreign);
        let socket = media.try_clone().context("cloning media socket")?;
        std::thread::Builder::new()
            .name("micbridge-media-rx".into())
            .spawn(move || media_loop(socket, sink, format, peer.ip(), stop, foreign))
            .context("spawning media thread")?
    };

    // Both sinks have to stay alive for the length of the session. A `cpal::Stream`
    // is not `Send`, so it is built and dropped on this thread; the WAV sink gets
    // its own thread because its loop blocks on `sleep`. Both were already resolved
    // above, so nothing here can fail for a reason the sender should have been told
    // about before the ack.
    let mut render_handle = None;
    let mut wav_handle = None;
    if let Some(target) = target {
        let render = render::start(target, source, state.level_meter(), state.gain())?;
        tracing::info!(device = %render.device_name, rate = render.sample_rate, channels = render.channels, "rendering");
        state.push_log(format!("rendering to {}", render.device_name));
        render_handle = Some(render);
    } else if let Some(sink) = wav_sink {
        let chunk = config.wav_chunk_frames as usize;
        let stop = Arc::clone(&session_stop);
        let level = state.level_meter();
        let gain = state.gain();
        tracing::info!(path = %output_name, "writing wav");
        state.push_log(format!("writing {output_name}"));
        wav_handle = Some(
            std::thread::Builder::new()
                .name("micbridge-wav".into())
                // The writer also raises the session's stop flag when it exits, so
                // a mid-session write failure is reported on the next beat rather
                // than only at join time — the receiver used to look healthy for
                // the whole session while writing nothing.
                .spawn({
                    let stop_on_exit = Arc::clone(&session_stop);
                    move || {
                        let outcome = micbridge_audio::wav::run(
                            sink,
                            source,
                            chunk,
                            output_rate,
                            stop,
                            level,
                            gain,
                        );
                        if outcome.is_err() {
                            stop_on_exit.store(true, Ordering::Relaxed);
                        }
                        outcome
                    }
                })
                .context("spawning wav thread")?,
        );
    }

    state.set_status(Status::Running);

    let result = control_loop(
        &mut control,
        &stats,
        format,
        output_rate,
        remaining,
        state,
        &session_stop,
        &mut reader,
    );

    session_stop.store(true, Ordering::Relaxed);
    let _ = media_thread.join();

    // Dropping the stream stops the device callback; do it before reporting so the
    // counters cannot move underneath the summary.
    drop(render_handle);

    if let Some(handle) = wav_handle {
        match handle.join() {
            Ok(Ok(frames)) => {
                tracing::info!(frames, "wav written");
                state.push_log(format!("wrote {frames} frames"));
            }
            // A failed write is worth reporting even when the session itself ended
            // cleanly — otherwise the file is silently short or missing.
            Ok(Err(err)) => {
                tracing::warn!("wav sink failed: {err:#}");
                state.push_log(format!("wav sink failed: {err:#}"));
            }
            Err(_) => tracing::warn!("wav thread panicked"),
        }
    }

    let foreign = foreign.load(Ordering::Relaxed);
    if foreign > 0 {
        tracing::warn!(datagrams = foreign, "ignored media from an address other than the sender");
        state.push_log(format!("ignored {foreign} datagrams from another address"));
    }
    report_final(&stats, output_rate, &output_name);

    result
}

fn render_config(config: &ReceiverConfig) -> RenderConfig {
    RenderConfig {
        device: match &config.sink {
            Sink::Device(device) => device.clone(),
            Sink::Wav(_) => None,
        },
        // Both `None` on purpose: the device's rate and channel count are
        // authoritative, and the pipeline adapts to them. Demanding the stream's
        // channel count instead is rejected outright by WASAPI in shared mode, which
        // only supports the endpoint's own — and because CoreAudio converts silently,
        // that failure appears exclusively on Windows.
        sample_rate: None,
        channels: None,
        buffer_frames: config.render_frames,
    }
}

fn validate(format: &StreamFormat, version: u32) -> std::result::Result<(), String> {
    if version != PROTOCOL_VERSION {
        return Err(format!(
            "sender speaks protocol {version}, this build speaks {PROTOCOL_VERSION}"
        ));
    }
    if format.channels == 0 {
        return Err("sender advertised zero channels".to_string());
    }
    if format.sample_rate == 0 {
        return Err("sender advertised a zero sample rate".to_string());
    }
    if format.frames_per_packet == 0 {
        return Err("sender advertised zero frames per packet".to_string());
    }
    if format.payload_bytes_per_packet() + micbridge_protocol::HEADER_LEN > MAX_DATAGRAM {
        return Err(format!(
            "sender's {}-byte datagrams exceed the {MAX_DATAGRAM}-byte receive buffer",
            format.payload_bytes_per_packet() + micbridge_protocol::HEADER_LEN
        ));
    }
    Ok(())
}

fn bind_media(bind: &str, preferred: u16) -> Result<UdpSocket> {
    match UdpSocket::bind((bind, preferred)) {
        Ok(socket) => Ok(socket),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            tracing::warn!(port = preferred, "media port is taken, using an ephemeral one");
            UdpSocket::bind((bind, 0)).context("binding an ephemeral media port")
        }
        Err(err) => Err(err).with_context(|| format!("binding media port {bind}:{preferred}")),
    }
}

/// Reads datagrams and hands their samples to the pipeline.
fn media_loop(
    socket: UdpSocket,
    mut sink: MediaSink,
    format: StreamFormat,
    expect_from: IpAddr,
    stop: Arc<AtomicBool>,
    foreign: Arc<AtomicU64>,
) {
    // A timeout rather than a blocking read, so the thread notices shutdown without
    // needing the socket to be closed underneath it.
    if let Err(err) = socket.set_read_timeout(Some(CONTROL_POLL)) {
        tracing::error!(%err, "could not set a media read timeout");
        return;
    }

    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut samples: Vec<i16> =
        Vec::with_capacity(format.frames_per_packet as usize * format.channels as usize);
    let mut warned_hid = false;

    while !stop.load(Ordering::Relaxed) {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(pair) => pair,
            Err(err) if is_timeout(&err) => continue,
            // On Windows a connected UDP socket surfaces an ICMP port-unreachable
            // from a previous send as ConnectionReset on the *next* receive. It says
            // nothing about this socket's health, so it must not end the loop.
            Err(err) if err.kind() == ErrorKind::ConnectionReset => continue,
            Err(err) => {
                tracing::warn!(%err, "media receive failed");
                continue;
            }
        };

        // With no authentication on the media channel, matching the control peer's
        // address is the only filter available. It is not security — a spoofed
        // source address defeats it — which is why the protocol document says LAN
        // and Tailscale only.
        if from.ip() != expect_from {
            foreign.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let (header, payload) = match micbridge_protocol::MediaHeader::decode(&buf[..len]) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::debug!(%err, "discarding a malformed datagram");
                continue;
            }
        };

        match header.channel {
            CHANNEL_AUDIO => {}
            CHANNEL_HID => {
                if !warned_hid {
                    tracing::warn!("sender is sending HID reports; this build only handles audio");
                    warned_hid = true;
                }
                continue;
            }
            other => {
                tracing::debug!(channel = other, "ignoring an unknown media channel");
                continue;
            }
        }

        if pcm::payload_to_i16(payload, &mut samples).is_none() {
            tracing::debug!(bytes = payload.len(), "discarding an odd-length payload");
            continue;
        }
        sink.accept(header.sample_idx, &samples);
    }
}

/// Sends heartbeats and statistics, and gives up if the sender goes quiet.
#[allow(clippy::too_many_arguments)]
fn control_loop(
    control: &mut TcpStream,
    stats: &Arc<PipelineStats>,
    format: StreamFormat,
    output_rate: u32,
    remaining: Option<Duration>,
    state: &Arc<SessionState>,
    session_stop: &Arc<AtomicBool>,
    reader: &mut FrameReader,
) -> Result<()> {
    // The ratio that would apply with two perfect clocks. Computed from the two
    // rates rather than sampled from the first statistics beat: sampling made the
    // reported trim relative to whatever correction happened to be in force a
    // second in, so it read as zero while the buffer was visibly off target.
    let nominal = format.sample_rate as f64 / output_rate.max(1) as f64;

    let started = Instant::now();
    let deadline = remaining.map(|r| started + r);
    let mut next_beat = Instant::now() + HEARTBEAT;
    let mut last_heard = Instant::now();

    loop {
        if state.stop_requested() || session_stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            tracing::info!(seconds = started.elapsed().as_secs(), "duration reached");
            return Ok(());
        }
        if last_heard.elapsed() > PEER_TIMEOUT {
            bail!("sender went quiet for {:?}", last_heard.elapsed());
        }

        if Instant::now() >= next_beat {
            next_beat += HEARTBEAT;
            let snapshot = stats.snapshot(output_rate);
            let trim = timing::ppm(snapshot.resample_ratio, nominal);
            tracing::info!(
                fill_ms = %format!("{:.1}", snapshot.buffer_fill_ms),
                trim_ppm = %format!("{trim:+.0}"),
                underruns = snapshot.underruns,
                overruns = snapshot.overruns,
                lost = snapshot.frames_lost,
                late = snapshot.packets_late,
                reordered = snapshot.packets_reordered,
                packets = snapshot.packets_received,
                "playing"
            );
            state.update_stats(|s| {
                s.packets = snapshot.packets_received;
                s.fill_ms = snapshot.buffer_fill_ms;
                s.trim_ppm = trim;
                s.underruns = snapshot.underruns;
                s.overruns = snapshot.overruns;
                s.frames_lost = snapshot.frames_lost;
                s.packets_late = snapshot.packets_late;
            });

            framing::send(control, &ServerMessage::Heartbeat).context("sending heartbeat")?;
            framing::send(control, &ServerMessage::Stats(snapshot)).context("sending stats")?;
        }

        // `Ok(None)` is a read timeout with any partial frame retained, which is the
        // normal idle case: it just means the loop gets a turn.
        match reader.poll_msg::<_, ClientMessage>(control) {
            Ok(None) => {}
            Ok(Some(ClientMessage::Goodbye)) => {
                tracing::info!("sender said goodbye");
                return Ok(());
            }
            Ok(Some(ClientMessage::Heartbeat)) => last_heard = Instant::now(),
            Ok(Some(ClientMessage::Stats(sender))) => {
                last_heard = Instant::now();
                if sender.frames_dropped > 0 {
                    tracing::warn!(
                        frames = sender.frames_dropped,
                        "sender is dropping capture frames"
                    );
                }
                state.update_stats(|s| s.frames_dropped = sender.frames_dropped);
            }
            Ok(Some(ClientMessage::Hello(_))) => bail!("sender sent a second hello"),
            Err(framing::Error::Io(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                tracing::info!("sender closed the connection");
                return Ok(());
            }
            Err(err) => return Err(err).context("control channel"),
        }
    }
}

fn report_final(stats: &Arc<PipelineStats>, output_rate: u32, sink_name: &str) {
    let snapshot = stats.snapshot(output_rate);
    // The soak test reads these numbers and nothing else.
    tracing::info!(
        sink = %sink_name,
        packets = snapshot.packets_received,
        underruns = snapshot.underruns,
        overruns = snapshot.overruns,
        lost = snapshot.frames_lost,
        late = snapshot.packets_late,
        reordered = snapshot.packets_reordered,
        "session summary"
    );
}
