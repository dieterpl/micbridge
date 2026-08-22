//! `micbridge` — the command-line frontend.
//!
//! `micbridge send` runs on the machine holding the audio interface; `micbridge recv` runs on
//! the machine running the game. They are the same executable because the only
//! platform-specific thing either does is ask cpal for a device, and keeping them
//! together means the wire format cannot drift between the two halves of a release.
//!
//! All the behaviour lives in `micbridge-engine`. This file turns arguments into a config
//! and waits.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use micbridge_engine::config::{DEFAULT_GAIN_DB, DEFAULT_PACKET_FRAMES, DEFAULT_TARGET_BUFFER_MS};
use micbridge_engine::{ReceiverConfig, SenderConfig, Session, Sink, Source, Status};
use micbridge_protocol::{DEFAULT_CONTROL_PORT, DEFAULT_MEDIA_PORT};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "micbridge",
    version,
    about = "Stream an audio input to a remote machine's virtual microphone"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Log filter, e.g. `debug` or `micbridge=debug,micbridge_core=trace`.
    #[arg(long, global = true, env = "MICBRIDGE_LOG", default_value = "info")]
    log: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Capture a local input device and stream it to a receiver.
    Send(SendArgs),
    /// Receive a stream and render it to a local output device or a file.
    Recv(RecvArgs),
    /// List the audio devices this machine can see.
    Devices,
    /// Look for receivers on the local network.
    Discover(DiscoverArgs),
    /// Start the receiver automatically at logon. Windows only.
    Autostart(AutostartArgs),
}

#[derive(Parser, Debug)]
struct AutostartArgs {
    #[command(subcommand)]
    action: AutostartAction,
}

#[derive(Subcommand, Debug)]
enum AutostartAction {
    /// Register this executable to start receiving at logon.
    Enable(AutostartEnableArgs),
    /// Remove the entry.
    Disable,
    /// Show what is registered.
    Status,
}

#[derive(Parser, Debug)]
struct AutostartEnableArgs {
    /// Microphone a game should hear, passed through to `recv`.
    #[arg(long, value_name = "NAME")]
    game_mic: Option<String>,

    /// Playback device to render into, if you would rather name it directly.
    #[arg(long, conflicts_with = "game_mic")]
    device: Option<String>,

    /// Jitter-buffer target to register.
    #[arg(long)]
    target_buffer_ms: Option<u32>,

    /// Control port to register.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Parser, Debug)]
struct DiscoverArgs {
    /// Discovery port to probe.
    #[arg(long, default_value_t = micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT)]
    port: u16,
}

#[derive(Parser, Debug)]
struct SendArgs {
    /// Receiver's address or hostname.
    ///
    /// Omit it to look for a receiver on the local network instead. Discovery only
    /// reaches the local segment, so over Tailscale or a routed network the address
    /// still has to be given.
    #[arg(long)]
    host: Option<String>,

    /// Receiver's control port.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    port: u16,

    /// Case-insensitive substring of the capture device name, e.g. `UMC204HD`.
    /// Omit to use the system default input.
    #[arg(long)]
    device: Option<String>,

    /// Send a sine wave at this frequency instead of capturing a device.
    ///
    /// The first thing to reach for when bringing this up across two machines: if
    /// `--tone 1000` moves the level meter on the receiving end, everything except
    /// the microphone works. Needs no device and no microphone permission.
    #[arg(long, value_name = "HZ", conflicts_with = "device")]
    tone: Option<f64>,

    /// Override the capture rate. Omit to accept the device's own rate, which
    /// avoids making CoreAudio resample before we even see the samples.
    #[arg(long)]
    rate: Option<u32>,

    /// Override the channel count. Omit to accept the device's own.
    #[arg(long)]
    channels: Option<u16>,

    /// Frames per datagram. 240 at 48 kHz is 5 ms, which keeps a stereo packet at
    /// 960 payload bytes — comfortably inside any path's MTU.
    #[arg(long, default_value_t = DEFAULT_PACKET_FRAMES)]
    packet_frames: u32,

    /// Capture callback size in frames. Omit to let the host decide.
    #[arg(long)]
    capture_frames: Option<u32>,

    /// Print the device and format that would be negotiated, then exit.
    #[arg(long)]
    probe: bool,

    /// Stop after this many seconds. Used by the soak and latency harnesses.
    #[arg(long)]
    duration_secs: Option<u64>,

    /// Amplify the audio, in decibels. Positive is louder; +6 is roughly twice as
    /// loud. Applied before the signal leaves this machine, so the level meter and
    /// the clip warning both reflect it.
    #[arg(long, default_value_t = DEFAULT_GAIN_DB, allow_negative_numbers = true)]
    gain_db: f32,
}

#[derive(Parser, Debug)]
struct RecvArgs {
    /// Address to accept a control connection on.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Control port.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    port: u16,

    /// Media port. Reported back to the sender in the handshake, so it does not
    /// have to be configured on both sides.
    #[arg(long, default_value_t = DEFAULT_MEDIA_PORT)]
    media_port: u16,

    /// Where received audio goes.
    #[arg(long, value_enum, default_value_t = SinkKind::Device)]
    sink: SinkKind,

    /// Output file for `--sink wav`.
    #[arg(long, default_value = "captures/micbridge.wav")]
    wav_path: PathBuf,

    /// Case-insensitive substring of the playback device to render into.
    ///
    /// This is the *playback* half of a virtual cable, not the microphone the game
    /// selects. Prefer `--game-mic`, which names the microphone and works the
    /// playback device out for you. `micbridge devices` lists both halves of each route.
    #[arg(long, conflicts_with = "game_mic")]
    device: Option<String>,

    /// Name the microphone a game should hear, and render into whatever feeds it.
    ///
    /// The right way round to think about it: you care which microphone the game
    /// picks up, and the playback device is an implementation detail. Choosing a
    /// playback device directly invites picking real speakers, which puts the audio in
    /// the room and leaves the game with no microphone at all.
    #[arg(long, value_name = "NAME")]
    game_mic: Option<String>,

    /// Jitter-buffer target. The dominant term in end-to-end latency and the budget
    /// for network jitter: 20 ms is comfortable on wired Ethernet, 10 ms is
    /// reachable, and below that a single late datagram is audible.
    #[arg(long, default_value_t = DEFAULT_TARGET_BUFFER_MS)]
    target_buffer_ms: u32,

    /// Amplify the audio, in decibels. Positive is louder; +6 is roughly twice as
    /// loud. Applied after the jitter buffer, just before the device, so it is the
    /// right knob when the audio is only too quiet at this end.
    #[arg(long, default_value_t = DEFAULT_GAIN_DB, allow_negative_numbers = true)]
    gain_db: f32,

    /// Render callback size in frames. Omit to let the host decide.
    #[arg(long)]
    render_frames: Option<u32>,

    /// Chunk size for `--sink wav`, standing in for a device callback.
    #[arg(long, default_value_t = DEFAULT_PACKET_FRAMES)]
    wav_chunk_frames: u32,

    /// Serve one session and exit, rather than waiting for the next sender.
    #[arg(long)]
    once: bool,

    /// Stop after this many seconds.
    #[arg(long)]
    duration_secs: Option<u64>,

    /// Do not answer discovery probes.
    ///
    /// Answering is on by default and is passive — it replies to probes on the local
    /// segment and initiates nothing.
    #[arg(long)]
    no_announce: bool,

    /// Port to answer discovery probes on.
    #[arg(long, default_value_t = micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT)]
    discovery_port: u16,

    /// A name to report in discovery replies, to tell two receivers apart.
    #[arg(long)]
    label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SinkKind {
    /// An output device — normally the playback half of a virtual cable.
    Device,
    /// A WAV file. Needs no audio hardware, which is what makes the receive path
    /// testable in CI and on a machine with nothing plugged in.
    Wav,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        // Decide colour rather than assuming it. The legacy Windows console host —
        // cmd.exe and PowerShell 5 on Windows 10, which is exactly where a
        // first-time user does bring-up — prints escape sequences literally, so
        // every line arrives wrapped in visible `<-[2m` garbage. This also keeps
        // escapes out of redirected log files on every platform.
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    match cli.command {
        Command::Send(args) => send(args),
        Command::Recv(args) => recv(args),
        Command::Devices => devices(),
        Command::Discover(args) => discover(args),
        Command::Autostart(args) => autostart(args),
    }
}

fn autostart(args: AutostartArgs) -> Result<()> {
    use micbridge_engine::autostart;

    if !autostart::supported() {
        // Say so rather than writing something that will never run. The Windows
        // registry's Run key has no counterpart here that this program implements.
        anyhow::bail!(
            "autostart is only implemented for Windows; run this on the machine \
             that receives"
        );
    }

    match args.action {
        AutostartAction::Status => {
            let status = autostart::status()?;
            if !status.enabled {
                println!("Not registered. `micbridge autostart enable` to add it.");
                return Ok(());
            }
            println!("Registered as {:?}:", autostart::ENTRY_NAME);
            println!("  {}", status.command.unwrap_or_default());
            if status.stale {
                println!();
                println!("Warning: that does not point at this executable. If the binary was");
                println!("moved or rebuilt elsewhere, the entry will fail silently at logon.");
                println!("Run `micbridge autostart enable` again to repoint it.");
            }
        }
        AutostartAction::Disable => {
            autostart::disable()?;
            println!("Removed. It will not start at logon.");
        }
        AutostartAction::Enable(enable) => {
            // Build the `recv` invocation to register. Whatever is passed here is
            // what runs at logon, so it is spelled out in full afterwards.
            let mut argv = vec!["recv".to_string()];
            if let Some(mic) = enable.game_mic {
                argv.push("--game-mic".into());
                argv.push(mic);
            } else if let Some(device) = enable.device {
                argv.push("--device".into());
                argv.push(device);
            }
            if let Some(ms) = enable.target_buffer_ms {
                argv.push("--target-buffer-ms".into());
                argv.push(ms.to_string());
            }
            if let Some(port) = enable.port {
                argv.push("--port".into());
                argv.push(port.to_string());
            }

            let command = autostart::enable(&argv)?;
            println!("Registered. At your next logon Windows will run:");
            println!("  {command}");
            println!();
            println!("This is the console build, so a terminal window will appear and stay");
            println!("open while it runs. For a windowed version instead, tick");
            println!("\"Start at login\" in micbridge-gui.");
            println!();
            println!("Remove it with `micbridge autostart disable`, or from Task Manager's");
            println!("Startup tab, where it appears as {:?}.", autostart::ENTRY_NAME);
        }
    }
    Ok(())
}

fn discover(args: DiscoverArgs) -> Result<()> {
    println!("Looking for receivers on the local network...");
    let found = micbridge_engine::discovery::find(args.port)?;

    if found.is_empty() {
        println!("None found.");
        println!();
        println!("Broadcast only reaches the local network segment. It does not cross");
        println!("Tailscale, a VPN, or a routed subnet, and some Wi-Fi access points");
        println!("filter it. Start the receiver and read the address it prints, then");
        println!("pass it with --host.");
        return Ok(());
    }

    println!();
    for receiver in &found {
        println!("  {}  ({})", receiver.address(), receiver.label);
        println!(
            "    micbridge send --host {} --port {} --tone 1000",
            receiver.address(),
            receiver.control_port
        );
    }
    println!();
    println!("Start with --tone: if the receiver's meter moves, everything except the");
    println!("microphone works. Then swap it for --device.");
    Ok(())
}

fn send(args: SendArgs) -> Result<()> {
    // No host given: go and look. Failing here with the reason beats connecting to
    // nothing and reporting a timeout.
    let (host, port) = match args.host {
        Some(host) => (host, args.port),
        None => {
            println!("No --host given, looking for a receiver...");
            let found = micbridge_engine::discovery::find(
                micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT,
            )?;
            let first = found.first().ok_or_else(|| {
                anyhow::anyhow!(
                    "no receiver found on the local network. Start one, then pass its address \
                     with --host — discovery does not cross Tailscale or a routed subnet"
                )
            })?;
            if found.len() > 1 {
                println!(
                    "Found {} receivers; using {}. Pass --host to choose.",
                    found.len(),
                    first.address()
                );
            } else {
                println!("Found {} ({})", first.address(), first.label);
            }
            (first.address(), first.control_port)
        }
    };

    let config = SenderConfig {
        host,
        port,
        source: match args.tone {
            Some(hz) => Source::Tone(hz),
            None => Source::Device(args.device),
        },
        sample_rate: args.rate,
        channels: args.channels,
        packet_frames: args.packet_frames,
        capture_frames: args.capture_frames,
        duration_secs: args.duration_secs,
        gain_db: args.gain_db,
    };

    if args.probe {
        let (device, format) = micbridge_engine::send::probe(&config)?;
        println!("{}", micbridge_audio::describe_default_devices());
        println!(
            "would capture {device:?} at {} Hz, {} channels, {} frames per packet ({:.2} ms, {} payload bytes)",
            format.sample_rate,
            format.channels,
            format.frames_per_packet,
            format.packet_ms(),
            format.payload_bytes_per_packet(),
        );
        return Ok(());
    }

    await_session(Session::start_sender(config))
}

fn recv(args: RecvArgs) -> Result<()> {
    if args.sink == SinkKind::Device {
        tracing::info!("{}", micbridge_audio::describe_default_devices());
    }

    // `--game-mic` names the microphone; resolve it to the playback device that feeds
    // it. Failing loudly here beats rendering somewhere the game cannot hear.
    let device = match (&args.game_mic, args.device.clone()) {
        (Some(wanted), _) => {
            let routes = micbridge_audio::virtual_device::detect()?;
            let needle = wanted.to_lowercase();
            let route = routes
                .iter()
                .find(|route| route.game_mic.to_lowercase().contains(&needle))
                .ok_or_else(|| {
                    let available: Vec<&str> = routes.iter().map(|r| r.game_mic.as_str()).collect();
                    anyhow::anyhow!(
                        "no feedable microphone matching {wanted:?}. Available: {}",
                        if available.is_empty() {
                            "none — install a virtual audio cable".to_string()
                        } else {
                            available.join(", ")
                        }
                    )
                })?;
            tracing::info!(
                game_mic = %route.game_mic,
                render_into = %route.render_into,
                "resolved microphone to its playback device"
            );
            Some(route.render_into.clone())
        }
        (None, device) => device,
    };

    await_session(Session::start_receiver(ReceiverConfig {
        bind: args.bind,
        port: args.port,
        media_port: args.media_port,
        sink: match args.sink {
            SinkKind::Device => Sink::Device(device),
            SinkKind::Wav => Sink::Wav(args.wav_path),
        },
        target_buffer_ms: args.target_buffer_ms,
        gain_db: args.gain_db,
        render_frames: args.render_frames,
        wav_chunk_frames: args.wav_chunk_frames,
        once: args.once,
        duration_secs: args.duration_secs,
        announce: !args.no_announce,
        discovery_port: args.discovery_port,
        label: args.label.unwrap_or_default(),
    }))
}

/// Waits for a session to finish, then turns its outcome into a process exit code.
///
/// The engine folds failures into the shared state rather than returning them, so
/// the CLI reads the final status instead of a `Result` — which is also what keeps
/// the GUI from having to join a thread to find out what went wrong.
fn await_session(session: Session) -> Result<()> {
    let state = session.state();
    session.join();

    match state.status() {
        Status::Failed(message) => Err(anyhow::anyhow!(message)),
        _ => Ok(()),
    }
}

fn devices() -> Result<()> {
    use micbridge_audio::devices::{list_devices, Direction};
    use micbridge_audio::virtual_device;

    println!("{}\n", micbridge_audio::describe_default_devices());

    for (direction, label) in [(Direction::Input, "Inputs"), (Direction::Output, "Outputs")] {
        println!("{label}:");
        // Report an enumeration failure rather than exiting on it. This is a
        // diagnostic command, and on a machine with no audio stack at all — a CI
        // runner, a headless box — "the host would not enumerate" is the answer the
        // user came for, not an error that should hide the other direction.
        match list_devices(direction) {
            Ok(listed) if listed.is_empty() => println!("  (none)"),
            Ok(listed) => {
                for name in listed {
                    println!("  {name}");
                }
            }
            Err(err) => println!("  (enumeration failed: {err:#})"),
        }
        println!();
    }

    // The part worth spelling out: which microphones an application can actually
    // feed, and which playback device reaches each one. Rendering into a real speaker
    // puts the audio in the room and leaves a game with no microphone at all, so the
    // useful listing is by microphone rather than by output.
    match virtual_device::detect() {
        Ok(routes) if routes.is_empty() => {
            println!("No microphone on this machine can be fed by an application.");
            println!("Install a virtual audio cable to make one, or use");
            println!("`micbridge recv --sink wav` to test without any.");
        }
        Ok(routes) => {
            println!("Microphones an application can feed:");
            for route in routes {
                println!("  {}", route.game_mic);
                println!("    micbridge recv --device {:?}", route.render_into);
                if !route.how.is_reliable() {
                    println!("    note: {}", route.how.describe());
                }
            }
            println!();
            println!("Run `recv` with the --device shown, then select the microphone above");
            println!("in your game. They are two halves of one route, not the same device.");
        }
        Err(err) => println!("(could not check: {err:#})"),
    }
    Ok(())
}
