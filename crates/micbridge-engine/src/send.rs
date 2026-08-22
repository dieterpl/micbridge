//! The sending end: capture a local input and stream it.
//!
//! Runs on one thread plus a media thread. The capture callback copies frames into
//! a lock-free ring and returns; the media thread packetises and sends; this
//! thread owns the control connection and the `cpal::Stream`, which on macOS is
//! not `Send` and so cannot be moved anywhere else.

use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use micbridge_audio::capture::{self, CaptureConfig};
use micbridge_core::pcm;
use micbridge_core::ring::{frame_channel, FrameConsumer};
use micbridge_core::Gain;
use micbridge_protocol::{
    framing, ClientMessage, FrameReader, Hello, SenderStats, ServerMessage, StreamFormat,
    CHANNEL_AUDIO, HEADER_LEN, PROTOCOL_VERSION,
};

use crate::config::{SenderConfig, Source};
use crate::state::{SessionState, Status};
use crate::timing::{self, CONTROL_POLL, HANDSHAKE_TIMEOUT, HEARTBEAT, MEDIA_POLL};

/// Capture ring depth. Two hundred milliseconds is far more than the media thread
/// needs, and the cost is a few hundred kilobytes; the point is that a scheduling
/// hiccup on the media thread never costs audio.
const CAPTURE_RING_MS: usize = 200;

/// The device name and format a sender would negotiate, without starting one.
pub fn probe(config: &SenderConfig) -> Result<(String, StreamFormat)> {
    match config.source {
        Source::Tone(hz) => Ok((
            format!("tone {hz} Hz"),
            micbridge_audio::tone::format(
                config.sample_rate,
                config.channels,
                config.packet_frames,
            ),
        )),
        Source::Device(ref device) => capture::probe(
            &CaptureConfig {
                device: device.clone(),
                sample_rate: config.sample_rate,
                channels: config.channels,
                buffer_frames: config.capture_frames,
                // Probing only negotiates a format; nothing is captured, so the
                // gain here is never applied to anything.
                gain: Arc::new(Gain::unity()),
            },
            config.packet_frames,
        ),
    }
}

/// Runs a send session to completion. Returns when it stops, is stopped, or fails.
pub fn run(config: &SenderConfig, state: &Arc<SessionState>) -> Result<()> {
    if config.host.trim().is_empty() {
        bail!("no host given — set the address of the machine running the receiver");
    }

    // The session's gain, not a fresh one: the UI holds the same Arc and turns it
    // while this is running.
    let gain = state.gain();
    gain.set_db(config.gain_db);

    let capture_config = CaptureConfig {
        device: match &config.source {
            Source::Device(device) => device.clone(),
            Source::Tone(_) => None,
        },
        sample_rate: config.sample_rate,
        channels: config.channels,
        buffer_frames: config.capture_frames,
        gain: Arc::clone(&gain),
    };

    state.set_status(Status::Starting("resolving source".into()));
    let (device_name, format) = probe(config)?;
    state.update_stats(|s| {
        s.sample_rate = format.sample_rate;
        s.channels = format.channels;
        s.endpoint = device_name.clone();
    });

    let control_addr = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {}:{}", config.host, config.port))?
        .next()
        .ok_or_else(|| anyhow!("{}:{} resolved to no addresses", config.host, config.port))?;

    state.set_status(Status::Starting(format!("connecting to {control_addr}")));
    tracing::info!(%control_addr, device = %device_name, rate = format.sample_rate, channels = format.channels, "connecting");

    let mut control = TcpStream::connect(control_addr)
        .with_context(|| format!("connecting to {control_addr}"))?;
    // Control messages are tiny and latency-sensitive; Nagle would batch a
    // heartbeat behind nothing and delay it for no gain.
    control.set_nodelay(true).context("setting TCP_NODELAY")?;

    framing::send(
        &mut control,
        &ClientMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            session_id: timing::new_session_id(),
            device_name: device_name.clone(),
            format,
        }),
    )
    .context("sending hello")?;

    // A read timeout from here on, and one `FrameReader` for the life of the
    // connection. The reader carries partial frames across timeouts; a fresh one per
    // read would lose the bytes already taken off the socket and desynchronise the
    // stream. The deadline stops a receiver that accepts and then says nothing from
    // hanging the sender forever.
    control.set_read_timeout(Some(CONTROL_POLL)).context("setting control read timeout")?;
    let mut reader = FrameReader::new();
    let ack = match reader
        .recv_until::<_, ServerMessage>(&mut control, Instant::now() + HANDSHAKE_TIMEOUT)
        .context("reading hello ack")?
    {
        ServerMessage::HelloAck(ack) => ack,
        ServerMessage::Error { message } => bail!("receiver rejected the session: {message}"),
        other => bail!("expected a hello ack, got {other:?}"),
    };

    if ack.format != format {
        bail!(
            "receiver wants {:?} but we offered {:?}; version 1 does not renegotiate",
            ack.format,
            format
        );
    }

    let media_addr = std::net::SocketAddr::new(control_addr.ip(), ack.media_port);
    tracing::info!(%media_addr, target_buffer_ms = ack.target_buffer_ms, "handshake complete");
    state.push_log(format!("handshake complete, media to {media_addr}"));

    let media = UdpSocket::bind(("0.0.0.0", 0)).context("binding media socket")?;
    // `connect` on a UDP socket fixes the destination, which lets the media thread
    // use `send` and lets the kernel report unreachable errors.
    media.connect(media_addr).with_context(|| format!("pointing media socket at {media_addr}"))?;

    let channels = format.channels as usize;
    let capacity_frames = format.sample_rate as usize * CAPTURE_RING_MS / 1000;
    let (producer, consumer) = frame_channel(channels, capacity_frames);

    let stop = state.stop_flag();
    let packets_sent = Arc::new(AtomicU64::new(0));
    let send_errors = Arc::new(AtomicU64::new(0));

    // A device stream and a tone thread differ in how they shut down but not in
    // what they feed, so nothing below cares which is running.
    let mut device = None;
    let mut tone = None;
    let frames_dropped = match config.source {
        Source::Tone(hz) => {
            state.set_status(Status::Starting(format!("generating a {hz} Hz tone")));
            let running = micbridge_audio::tone::start(
                hz,
                format,
                producer,
                Arc::clone(&stop),
                state.level_meter(),
            )?;
            tracing::info!(hz, "generating a tone instead of capturing");
            let dropped = Arc::clone(&running.frames_dropped);
            tone = Some(running);
            dropped
        }
        Source::Device(_) => {
            state.set_status(Status::Starting(format!("opening {device_name}")));
            let capture = capture::start_capture(
                &capture_config,
                config.packet_frames,
                producer,
                state.level_meter(),
            )?;
            tracing::info!(device = %capture.device_name, "capturing");
            let dropped = Arc::clone(&capture.frames_dropped);
            device = Some(capture);
            dropped
        }
    };

    let media_thread = {
        let stop = Arc::clone(&stop);
        let packets_sent = Arc::clone(&packets_sent);
        let send_errors = Arc::clone(&send_errors);
        std::thread::Builder::new()
            .name("micbridge-media-tx".into())
            .spawn(move || media_loop(consumer, media, format, stop, packets_sent, send_errors))
            .context("spawning media thread")?
    };

    state.set_status(Status::Running);

    let result = control_loop(
        &mut control,
        config,
        state,
        &frames_dropped,
        &packets_sent,
        &send_errors,
        &mut reader,
    );

    state.request_stop();
    let _ = media_thread.join();
    // Dropping the stream stops the device callback; the tone thread watches the
    // stop flag and is joined.
    drop(device);
    if let Some(tone) = tone {
        let _ = tone.thread.join();
    }

    let dropped = frames_dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        tracing::warn!(frames = dropped, "capture ring overflowed — the media thread fell behind");
        state.push_log(format!("{dropped} capture frames dropped"));
    }
    let sent = packets_sent.load(Ordering::Relaxed);
    tracing::info!(packets = sent, "stopped");
    state.push_log(format!("sent {sent} packets"));

    result
}

/// Packetises whole datagrams out of the capture ring and sends them.
fn media_loop(
    mut consumer: FrameConsumer,
    socket: UdpSocket,
    format: StreamFormat,
    stop: Arc<std::sync::atomic::AtomicBool>,
    packets_sent: Arc<AtomicU64>,
    send_errors: Arc<AtomicU64>,
) {
    let frames = format.frames_per_packet as usize;
    let channels = format.channels as usize;

    let mut frame_buf = vec![0.0f32; frames * channels];
    let mut pcm_buf: Vec<i16> = Vec::with_capacity(frames * channels);
    let mut datagram: Vec<u8> = Vec::with_capacity(HEADER_LEN + format.payload_bytes_per_packet());

    let mut seq: u32 = 0;
    let mut sample_idx: u64 = 0;
    let mut logged_error = false;

    while !stop.load(Ordering::Relaxed) {
        if consumer.occupied_frames() < frames {
            std::thread::sleep(MEDIA_POLL);
            continue;
        }

        let got = consumer.pop_frames(&mut frame_buf);
        debug_assert_eq!(got, frames, "occupancy was checked before the read");

        pcm::encode_into(&frame_buf, &mut pcm_buf);

        datagram.clear();
        datagram.extend_from_slice(
            &micbridge_protocol::MediaHeader::new(CHANNEL_AUDIO, seq, sample_idx).encode(),
        );
        pcm::i16_to_payload(&pcm_buf, &mut datagram);

        match socket.send(&datagram) {
            Ok(_) => {
                packets_sent.fetch_add(1, Ordering::Relaxed);
                logged_error = false;
            }
            Err(err) => {
                send_errors.fetch_add(1, Ordering::Relaxed);
                // Log the first of a run only. A send that fails usually fails two
                // hundred times a second, and filling the log with it would bury
                // whatever caused it.
                if !logged_error {
                    tracing::warn!(%err, "media send failed");
                    logged_error = true;
                }
            }
        }

        // `sample_idx` advances by the frames sent whether or not the datagram made
        // it, so a lost packet leaves a gap the receiver can see rather than
        // silently shifting everything after it earlier in time.
        seq = seq.wrapping_add(1);
        sample_idx += frames as u64;
    }
}

/// Sends heartbeats and statistics, and folds whatever the receiver reports into
/// the shared state.
fn control_loop(
    control: &mut TcpStream,
    config: &SenderConfig,
    state: &Arc<SessionState>,
    frames_dropped: &Arc<AtomicU64>,
    packets_sent: &Arc<AtomicU64>,
    send_errors: &Arc<AtomicU64>,
    reader: &mut FrameReader,
) -> Result<()> {
    let started = Instant::now();
    let deadline = config.duration_secs.map(|s| started + Duration::from_secs(s));
    let mut next_beat = Instant::now();

    loop {
        if state.stop_requested() {
            let _ = framing::send(control, &ClientMessage::Goodbye);
            return Ok(());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            tracing::info!(seconds = started.elapsed().as_secs(), "duration reached");
            let _ = framing::send(control, &ClientMessage::Goodbye);
            return Ok(());
        }

        if Instant::now() >= next_beat {
            next_beat += HEARTBEAT;
            let sent = packets_sent.load(Ordering::Relaxed);
            let dropped = frames_dropped.load(Ordering::Relaxed);
            state.update_stats(|s| {
                s.packets = sent;
                s.frames_dropped = dropped;
            });

            framing::send(control, &ClientMessage::Heartbeat).context("sending heartbeat")?;
            framing::send(
                control,
                &ClientMessage::Stats(SenderStats {
                    // Derived from packets rather than counted separately: every
                    // datagram carries exactly `packet_frames`, so a second counter
                    // could only disagree with this one.
                    frames_captured: sent * u64::from(config.packet_frames),
                    packets_sent: sent,
                    frames_dropped: dropped,
                }),
            )
            .context("sending stats")?;
        }

        // `Ok(None)` is a read timeout with any partial frame retained, which is the
        // normal idle case: it just means the loop gets a turn.
        match reader.poll_msg::<_, ServerMessage>(control) {
            Ok(None) => {}
            Ok(Some(ServerMessage::Stats(stats))) => {
                tracing::info!(
                    fill_ms = %format!("{:.1}", stats.buffer_fill_ms),
                    underruns = stats.underruns,
                    overruns = stats.overruns,
                    lost = stats.frames_lost,
                    late = stats.packets_late,
                    "receiver"
                );
                // The sender has no view of the buffer, so the receiver's numbers
                // are the only ones worth showing on this end.
                state.update_stats(|s| {
                    s.fill_ms = stats.buffer_fill_ms;
                    s.underruns = stats.underruns;
                    s.overruns = stats.overruns;
                    s.frames_lost = stats.frames_lost;
                    s.packets_late = stats.packets_late;
                });
            }
            Ok(Some(ServerMessage::Heartbeat)) => {}
            Ok(Some(ServerMessage::Error { message })) => bail!("receiver reported: {message}"),
            Ok(Some(ServerMessage::HelloAck(_))) => bail!("receiver sent a second hello ack"),
            Err(framing::Error::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::info!("receiver closed the connection");
                return Ok(());
            }
            Err(err) => return Err(err).context("control channel"),
        }

        let errors = send_errors.load(Ordering::Relaxed);
        if errors > 0 && errors.is_multiple_of(1_000) {
            tracing::warn!(errors, "media sends are failing");
        }
    }
}
