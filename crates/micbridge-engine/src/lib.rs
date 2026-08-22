//! Runs a send or receive session on a background thread and exposes its live
//! state.
//!
//! This exists so the CLI and the GUI are two thin frontends over one
//! implementation, rather than two implementations that drift apart. Everything
//! about *what a session does* lives here; everything about how it is asked for
//! and displayed lives in the frontends.
//!
//! The session runs on its own thread because a `cpal::Stream` is not `Send` on
//! macOS — the stream has to be created, held and dropped on one thread, and that
//! thread also runs the control loop. Watchers only ever read [`SessionState`],
//! which is safe to poll from a GUI paint loop.

pub mod autostart;
pub mod config;
pub mod discovery;
pub mod recv;
pub mod send;
pub mod state;
pub mod timing;

use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;

pub use config::{ReceiverConfig, SenderConfig, Sink, Source};
pub use state::{SessionState, Snapshot, Stats, Status};

/// A running session.
///
/// Dropping the handle does **not** stop the session — the thread owns everything
/// it needs. Call [`Session::stop`] and then [`Session::join`], or use
/// [`Session::stop_and_join`].
pub struct Session {
    state: Arc<SessionState>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Starts a sender on a background thread.
    pub fn start_sender(config: SenderConfig) -> Self {
        Self::spawn("micbridge-sender", move |state| send::run(&config, &state))
    }

    /// Starts a receiver on a background thread.
    pub fn start_receiver(config: ReceiverConfig) -> Self {
        Self::spawn("micbridge-receiver", move |state| recv::run(&config, &state))
    }

    fn spawn<F>(name: &str, body: F) -> Self
    where
        F: FnOnce(Arc<SessionState>) -> Result<()> + Send + 'static,
    {
        let state = Arc::new(SessionState::new());

        // Set on this thread, before spawning, so a caller that polls immediately
        // never sees `Idle`. Leaving it to the session body created a window where
        // the status said `Idle` — indistinguishable from "finished" — and anything
        // gating a Stop button on `is_active` would switch itself off at once.
        // `Idle` now means only "never started".
        state.set_status(Status::Starting("initialising".into()));

        let thread_state = Arc::clone(&state);

        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                // The result is folded into the shared state rather than returned,
                // so a frontend never has to join to find out what happened. The
                // `{:#}` is deliberate: with `anyhow` the cause chain is usually the
                // useful part, and the summary alone is often just "control channel".
                match body(Arc::clone(&thread_state)) {
                    Ok(()) => thread_state.set_status(Status::Stopped),
                    Err(err) => thread_state.set_status(Status::Failed(format!("{err:#}"))),
                }
            })
            .expect("spawning a session thread");

        Self { state, thread: Some(thread) }
    }

    /// The live state. Cheap to clone and safe to poll from a paint loop.
    pub fn state(&self) -> Arc<SessionState> {
        Arc::clone(&self.state)
    }

    /// Asks the session to wind down. Returns immediately.
    pub fn stop(&self) {
        self.state.request_stop();
    }

    /// True while the session thread is still doing something.
    pub fn is_active(&self) -> bool {
        self.state.status().is_active()
    }

    /// Waits for the session thread to finish.
    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Asks the session to stop and waits for it.
    pub fn stop_and_join(self) {
        self.stop();
        self.join();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Waits for a predicate, so a test never sleeps longer than it must and never
    /// depends on a fixed timing guess.
    fn wait_for(
        state: &Arc<SessionState>,
        timeout: Duration,
        f: impl Fn(&Status) -> bool,
    ) -> Status {
        let deadline = Instant::now() + timeout;
        loop {
            let status = state.status();
            if f(&status) || Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_sender_with_no_host_fails_with_a_useful_message() {
        let session = Session::start_sender(SenderConfig {
            source: Source::Tone(1_000.0),
            ..Default::default()
        });
        let state = session.state();
        let status = wait_for(&state, Duration::from_secs(5), |s| !s.is_active());

        match status {
            Status::Failed(message) => {
                assert!(message.contains("no host"), "unhelpful message: {message}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        session.join();
    }

    #[test]
    fn a_sender_pointed_at_nothing_reports_the_connection_failure() {
        // Port 1 with nothing listening: the failure has to reach the state rather
        // than only being logged, or a GUI would sit on "Starting" forever.
        let session = Session::start_sender(SenderConfig {
            host: "127.0.0.1".into(),
            port: 1,
            source: Source::Tone(1_000.0),
            ..Default::default()
        });
        let state = session.state();
        let status = wait_for(&state, Duration::from_secs(10), |s| !s.is_active());

        match status {
            Status::Failed(message) => {
                assert!(
                    message.to_lowercase().contains("connect"),
                    "should name the connection attempt: {message}"
                );
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        session.join();
    }

    #[test]
    fn a_receiver_can_be_stopped_while_waiting_for_a_sender() {
        // The Stop button has to work before anyone connects. A blocking `accept`
        // would leave this hanging, which is exactly the bug this pins down.
        let session = Session::start_receiver(ReceiverConfig {
            port: 0, // any free port
            media_port: 0,
            sink: Sink::Wav(std::env::temp_dir().join("micbridge-stop-test.wav")),
            ..Default::default()
        });
        let state = session.state();
        wait_for(&state, Duration::from_secs(5), |s| matches!(s, Status::Starting(_)));

        let stopping = Instant::now();
        session.stop();
        let status = wait_for(&state, Duration::from_secs(5), |s| !s.is_active());
        let took = stopping.elapsed();

        assert_eq!(status, Status::Stopped, "should have stopped cleanly");
        assert!(took < Duration::from_secs(2), "stop took {took:?}, should be prompt");
        session.join();
    }

    /// Waits for the receiver to publish the port it actually bound.
    ///
    /// Tests must not hard-code a port: two runs back to back leave the previous
    /// socket in TIME_WAIT, and a fixed port makes the second one fail for reasons
    /// that have nothing to do with the code. Binding zero and asking where it
    /// landed is the only reliable way.
    fn bound_port(state: &Arc<SessionState>, timeout: Duration) -> u16 {
        let deadline = Instant::now() + timeout;
        loop {
            let addr = state.stats().local_addr;
            if let Some(port) = addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
                if port != 0 {
                    return port;
                }
            }
            assert!(Instant::now() < deadline, "receiver never reported a bound address");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_full_session_moves_audio_and_reports_it() {
        let wav = std::env::temp_dir().join("micbridge-engine-roundtrip.wav");
        let _ = std::fs::remove_file(&wav);

        let receiver = Session::start_receiver(ReceiverConfig {
            // Port zero, then ask what was bound: no fixed port to collide with.
            port: 0,
            media_port: 0,
            sink: Sink::Wav(wav.clone()),
            once: true,
            // Six times the 20 ms default. The assertion below is `underruns == 0`,
            // and on a shared CI runner the sender's pacing is jittery enough to
            // starve a 20 ms buffer — which says something about the runner, not
            // about this code. The invariant worth pinning is that audio arrives
            // continuously when the buffer is sized for the link; whether 20 ms is
            // enough on real hardware is what the soak run in docs/testing.md
            // measures.
            target_buffer_ms: 120,
            ..Default::default()
        });
        let rx_state = receiver.state();
        let port = bound_port(&rx_state, Duration::from_secs(5));

        let sender = Session::start_sender(SenderConfig {
            host: "127.0.0.1".into(),
            port,
            source: Source::Tone(1_000.0),
            ..Default::default()
        });
        let tx_state = sender.state();

        let status = wait_for(&tx_state, Duration::from_secs(10), |s| *s == Status::Running);
        assert_eq!(status, Status::Running, "sender should have connected");
        assert_eq!(
            wait_for(&rx_state, Duration::from_secs(5), |s| *s == Status::Running),
            Status::Running,
            "receiver should have negotiated"
        );

        // Long enough for a couple of statistics beats.
        std::thread::sleep(Duration::from_millis(2_500));

        let rx = rx_state.snapshot();
        assert!(rx.stats.packets > 100, "receiver saw only {} packets", rx.stats.packets);
        assert_eq!(rx.stats.underruns, 0, "underran over loopback");
        assert_eq!(rx.stats.overruns, 0, "overran over loopback");
        assert!(rx.stats.fill_ms > 5.0, "buffer never filled: {} ms", rx.stats.fill_ms);
        assert!(rx.level > 0.1, "level meter should show the tone, read {}", rx.level);
        assert!(!rx.stats.endpoint.is_empty(), "endpoint should be named");
        assert!(rx.stats.local_addr.ends_with(&port.to_string()), "should report where it bound");

        let tx = tx_state.snapshot();
        assert!(tx.stats.packets > 100, "sender sent only {}", tx.stats.packets);
        assert_eq!(tx.stats.frames_dropped, 0, "sender dropped capture frames");
        assert!(tx.level > 0.1, "sender level meter should show the tone, read {}", tx.level);
        assert_eq!(tx.stats.sample_rate, 48_000);

        sender.stop_and_join();
        receiver.stop_and_join();
        let _ = std::fs::remove_file(&wav);
    }
}
