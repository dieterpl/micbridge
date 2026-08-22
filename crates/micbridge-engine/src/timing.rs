//! Session timing constants and small shared helpers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often each side sends a heartbeat, and how often the receiver reports
/// statistics.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

/// How long to wait without hearing anything before declaring the peer gone.
///
/// Four heartbeats. Generous, because tearing a session down costs a
/// re-prebuffer and throws away the accumulated clock-drift estimate, and a
/// momentarily busy machine is not a disconnect.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a control read blocks before the loop gets a turn to send its own
/// heartbeat and check for shutdown.
pub const CONTROL_POLL: Duration = Duration::from_millis(200);

/// How often the receiver checks for an incoming connection while idle.
pub const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// How long to wait for the peer's half of the handshake.
///
/// Without a bound, a peer that accepts the TCP connection and then says nothing —
/// a half-open NAT mapping, a receiver wedged on a device that will not open —
/// hangs the other side indefinitely with no way to tell it apart from a slow link.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the media thread sleeps when it has nothing to do.
///
/// Half a millisecond: short enough that it adds no meaningful latency to a 5 ms
/// packet, long enough not to be a spin loop. A condition variable would be
/// tighter, but the producer is a realtime audio callback and signalling one from
/// a callback is exactly the kind of blocking call that must not happen there.
pub const MEDIA_POLL: Duration = Duration::from_micros(500);

/// An identifier that distinguishes a reconnect from a second concurrent sender.
///
/// Wall-clock nanoseconds mixed with the process id, plus a process-local counter.
/// This does not need to be unpredictable — it is not a credential — only unlikely
/// to repeat between two senders on the same network.
///
/// The counter is not decoration: `SystemTime` on macOS is not actually
/// nanosecond-granular, so two calls in quick succession read the same value and
/// would otherwise produce the same id. Added rather than XORed, because XOR of a
/// low-bit counter into a clock that has also advanced by a bit or two can land
/// back on a value already handed out.
pub fn new_session_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos.wrapping_add(seq) ^ (u64::from(std::process::id()) << 48)
}

/// Formats a resample ratio as a trim in parts per million relative to nominal.
///
/// The raw ratio is unreadable at a glance — 1.0000617 against 0.9187500 tells
/// you nothing — whereas "+62 ppm" is directly the two clocks' disagreement.
pub fn ppm(ratio: f64, nominal: f64) -> f64 {
    if nominal == 0.0 {
        return 0.0;
    }
    (ratio / nominal - 1.0) * 1e6
}

/// True when a socket error is really just a read timeout.
///
/// A read timeout surfaces as `WouldBlock` on Unix and `TimedOut` on Windows, and
/// both mean the same thing: nothing arrived, carry on. Getting this wrong on one
/// platform turns every idle poll into a session failure, which is exactly the
/// class of bug that only shows up on the machine you did not test on.
pub fn is_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_differ_between_calls() {
        // Back-to-back calls are the hard case: the wall clock does not
        // necessarily advance between them.
        let ids: std::collections::HashSet<u64> = (0..1_000).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 1_000, "session ids collided");
    }

    #[test]
    fn ppm_reads_as_a_trim_relative_to_nominal() {
        assert!((ppm(1.0, 1.0)).abs() < 1e-9);
        assert!((ppm(1.0001, 1.0) - 100.0).abs() < 1e-6);
        // Works against a converting nominal ratio too.
        assert!((ppm(0.918_75 * 1.0002, 0.918_75) - 200.0).abs() < 1e-6);
    }

    #[test]
    fn ppm_is_safe_against_a_zero_nominal() {
        assert_eq!(ppm(1.0, 0.0), 0.0);
    }

    #[test]
    fn both_platforms_timeout_kinds_are_recognised() {
        use std::io::{Error, ErrorKind};

        assert!(is_timeout(&Error::from(ErrorKind::WouldBlock)), "Unix read timeout");
        assert!(is_timeout(&Error::from(ErrorKind::TimedOut)), "Windows read timeout");
        assert!(!is_timeout(&Error::from(ErrorKind::ConnectionReset)));
        assert!(!is_timeout(&Error::from(ErrorKind::UnexpectedEof)));
    }

    #[test]
    fn peer_timeout_is_several_heartbeats() {
        assert!(PEER_TIMEOUT >= HEARTBEAT * 3, "one slow beat must not drop a session");
    }
}
