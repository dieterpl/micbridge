//! Live session state, shared between the session thread and whoever is watching.
//!
//! The GUI polls this thirty times a second; the session thread writes to it once
//! a second, plus the level meter which is written from the audio callback. Nothing
//! here is on a realtime path except [`LevelMeter`], which is lock-free by
//! construction — so the rest can use a plain `Mutex` without apology.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use micbridge_core::{Gain, LevelMeter};

/// How many log lines are kept for display. Enough to see a session start and
/// fail; the full log goes to `tracing` regardless.
const LOG_CAPACITY: usize = 200;

/// Where a session has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not started.
    Idle,
    /// Opening devices, connecting, or waiting for a sender to appear.
    Starting(String),
    /// Handshake done and audio moving.
    Running,
    /// Ended cleanly.
    Stopped,
    /// Ended with an error. The string is the full `anyhow` chain, because the
    /// useful part is usually the cause rather than the summary.
    Failed(String),
}

impl Status {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Starting(_) | Self::Running)
    }

    /// A short label for display.
    pub fn label(&self) -> &str {
        match self {
            Self::Idle => "Idle",
            Self::Starting(_) => "Starting",
            Self::Running => "Running",
            Self::Stopped => "Stopped",
            Self::Failed(_) => "Failed",
        }
    }
}

/// Numbers worth showing. One struct for both directions; the fields the current
/// direction does not produce stay zero, which is simpler than two shapes and two
/// display paths.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub packets: u64,
    pub fill_ms: f32,
    /// Resampler trim relative to nominal, in parts per million. A direct estimate
    /// of how far the two machines' clocks disagree.
    pub trim_ppm: f64,
    pub underruns: u64,
    pub overruns: u64,
    pub frames_lost: u64,
    pub packets_late: u64,
    /// Sender side: capture frames dropped because the network thread fell behind.
    pub frames_dropped: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// What the session is actually talking to — a device name, or a file path.
    pub endpoint: String,
    /// The recording device a game should select as its microphone.
    ///
    /// Carried separately from `endpoint` because they are different devices and
    /// confusing them is the usual cause of a silent-but-apparently-working setup:
    /// the receiver renders into VB-CABLE's "CABLE Input", and the game has to
    /// listen to "CABLE Output". Empty when the sink is not a recognised cable.
    pub game_device: String,
    /// Addresses a peer could reach this receiver on.
    ///
    /// Reported because `local_addr` is usually `0.0.0.0`, which is true and useless:
    /// the number the user has to type on the *other* machine is never the bind
    /// address. Works even where broadcast discovery does not, which is the case that
    /// matters over Tailscale.
    pub reachable: Vec<String>,
    /// The receiver's actual listening address, once bound.
    ///
    /// Reported because the configured port and the bound port can differ, and
    /// because it is the thing the user has to type on the other machine. Empty on
    /// the sending side.
    pub local_addr: String,
}

/// Everything a watcher can see, captured at one moment.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: Status,
    pub stats: Stats,
    /// Peak magnitude since the previous snapshot, in `[0, 1]`.
    pub level: f32,
    pub log: Vec<String>,
    /// Seconds since the session thread started.
    pub elapsed_secs: f64,
}

/// Converts a linear peak magnitude to dBFS, floored at `floor_db`.
///
/// Re-exported here so a frontend can draw a meter without depending on
/// `micbridge-core` directly.
pub fn level_dbfs(magnitude: f32, floor_db: f32) -> f32 {
    LevelMeter::to_dbfs(magnitude, floor_db)
}

/// Shared, observable session state.
pub struct SessionState {
    /// Held behind an `Arc` rather than inline so it can be handed straight to
    /// the tone generator and the WAV sink, which already take a stop flag. One
    /// flag for the whole session means there is no way for half of it to keep
    /// running after a stop.
    stop: Arc<AtomicBool>,
    status: Mutex<Status>,
    stats: Mutex<Stats>,
    log: Mutex<VecDeque<String>>,
    /// Written from the audio callback, so it must stay lock-free.
    level: Arc<LevelMeter>,
    /// Read from the audio callback, and written from the UI while a session runs,
    /// so it must stay lock-free too.
    gain: Arc<Gain>,
    started: Instant,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            status: Mutex::new(Status::Idle),
            stats: Mutex::new(Stats::default()),
            log: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
            level: Arc::new(LevelMeter::new()),
            gain: Arc::new(Gain::unity()),
            started: Instant::now(),
        }
    }

    /// The meter the audio callbacks write into. Handed to the capture, render,
    /// tone and WAV paths so all four report through one mechanism.
    pub fn level_meter(&self) -> Arc<LevelMeter> {
        Arc::clone(&self.level)
    }

    /// The session's gain, shared with whichever audio path is running.
    ///
    /// Exposed rather than fixed at start-up so a slider can be moved mid-session:
    /// the alternative is tearing the audio device down and back up to change a
    /// multiply, which would be an audible gap for no reason.
    pub fn gain(&self) -> Arc<Gain> {
        Arc::clone(&self.gain)
    }

    /// The session's single stop flag, for the worker threads that take one.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Clears the stop flag so the same state can drive another session.
    pub fn rearm(&self) {
        self.stop.store(false, Ordering::Relaxed);
    }

    pub fn set_status(&self, status: Status) {
        // Log every transition, so the GUI's log pane explains what happened
        // without the user having to read a terminal.
        match &status {
            Status::Starting(what) => self.push_log(format!("starting: {what}")),
            Status::Running => self.push_log("running".to_string()),
            Status::Stopped => self.push_log("stopped".to_string()),
            Status::Failed(err) => self.push_log(format!("failed: {err}")),
            Status::Idle => {}
        }
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_stats(&self, stats: Stats) {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner()) = stats;
    }

    /// Applies a change to the current stats without replacing the whole struct,
    /// so the sender's and receiver's separate updates do not clobber each other.
    pub fn update_stats(&self, f: impl FnOnce(&mut Stats)) {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut stats);
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn push_log(&self, line: String) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() >= LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(line);
    }

    /// Reads everything at once. Takes the level meter's peak, so consecutive
    /// calls each report the interval since the last.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            status: self.status(),
            stats: self.stats(),
            level: self.level.take(),
            log: self.log.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect(),
            elapsed_secs: self.started.elapsed().as_secs_f64(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_idle_and_not_stopping() {
        let state = SessionState::new();
        assert_eq!(state.status(), Status::Idle);
        assert!(!state.stop_requested());
        assert!(!state.status().is_active());
    }

    #[test]
    fn stop_can_be_requested_and_rearmed() {
        let state = SessionState::new();
        state.request_stop();
        assert!(state.stop_requested());
        state.rearm();
        assert!(!state.stop_requested(), "the same state must be reusable for a second session");
    }

    #[test]
    fn status_transitions_are_logged() {
        let state = SessionState::new();
        state.set_status(Status::Starting("connecting".into()));
        state.set_status(Status::Running);
        state.set_status(Status::Failed("boom".into()));

        let snapshot = state.snapshot();
        assert_eq!(snapshot.status, Status::Failed("boom".into()));
        assert!(snapshot.log.iter().any(|l| l.contains("connecting")));
        assert!(snapshot.log.iter().any(|l| l.contains("boom")));
    }

    #[test]
    fn active_covers_starting_and_running_only() {
        assert!(Status::Starting(String::new()).is_active());
        assert!(Status::Running.is_active());
        assert!(!Status::Idle.is_active());
        assert!(!Status::Stopped.is_active());
        assert!(!Status::Failed(String::new()).is_active());
    }

    #[test]
    fn the_log_is_bounded() {
        let state = SessionState::new();
        for i in 0..LOG_CAPACITY * 3 {
            state.push_log(format!("line {i}"));
        }
        let log = state.snapshot().log;
        assert_eq!(log.len(), LOG_CAPACITY, "log must not grow without bound");
        // The newest lines are the ones kept.
        assert!(log.last().expect("non-empty").contains(&format!("{}", LOG_CAPACITY * 3 - 1)));
    }

    #[test]
    fn update_stats_is_a_partial_edit() {
        let state = SessionState::new();
        state.update_stats(|s| s.packets = 5);
        state.update_stats(|s| s.underruns = 2);
        let stats = state.stats();
        assert_eq!(stats.packets, 5, "the second update must not have cleared the first");
        assert_eq!(stats.underruns, 2);
    }

    #[test]
    fn snapshot_drains_the_level_meter() {
        let state = SessionState::new();
        state.level_meter().record(&[0.6]);
        assert!((state.snapshot().level - 0.6).abs() < 1e-6);
        assert_eq!(state.snapshot().level, 0.0, "each snapshot covers one interval");
    }

    #[test]
    fn a_poisoned_lock_does_not_wedge_the_ui() {
        // If a session thread panics while holding a lock, the GUI must keep
        // rendering rather than panicking in its own paint loop.
        let state = Arc::new(SessionState::new());
        let poisoner = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                state.set_status(Status::Running);
                let _guard = state.status.lock().unwrap();
                panic!("poisoning the lock on purpose");
            })
        };
        assert!(poisoner.join().is_err(), "the helper thread was supposed to panic");

        assert_eq!(state.status(), Status::Running);
        let _ = state.snapshot();
    }
}
