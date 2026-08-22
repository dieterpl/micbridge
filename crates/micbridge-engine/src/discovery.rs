//! Finding a receiver on the local network, and reporting where one can be reached.
//!
//! Two mechanisms, because neither is sufficient alone:
//!
//! * **Broadcast discovery.** A sender broadcasts a probe; receivers answer with the
//!   control port to use. Zero configuration, but broadcast only reaches one network
//!   segment — it does not cross Tailscale, a VPN, or a routed subnet, and some Wi-Fi
//!   access points filter it.
//! * **Reporting local addresses.** The receiver works out which of its own addresses
//!   a peer could reach it on and says so. Less magical, but it works everywhere
//!   broadcast does not, which is exactly the case that matters over Tailscale.
//!
//! The local-address trick avoids enumerating interfaces, which would need
//! platform-specific code on both targets. Instead it `connect`s a UDP socket toward
//! a representative destination and asks the socket which local address the kernel
//! picked. `connect` on UDP sends no packets — it only sets a default destination —
//! so this is instant, silent, and needs nothing but `std::net`.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use micbridge_protocol::discovery::Message;

/// How long a probe waits for replies. Long enough for a busy machine to answer,
/// short enough that a "Find" button does not feel broken.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(700);

/// How often the probe is repeated within that window, since a broadcast datagram is
/// unacknowledged and a single one can simply be dropped.
const PROBE_REPEATS: u32 = 3;

/// How long the responder blocks before re-checking its stop flag.
const RESPONDER_POLL: Duration = Duration::from_millis(200);

/// How long the prober sleeps between sweeps of its sockets.
const PROBE_POLL: Duration = Duration::from_millis(10);

/// How long to wait for a control port to accept a connection.
///
/// Short: this runs when discovery has already failed, and it is checking an
/// address on the same LAN or a Tailscale link, not the open internet.
const CONTROL_CHECK_TIMEOUT: Duration = Duration::from_millis(600);

/// Destinations used only to ask the routing table which local address it would use.
///
/// No packets are sent to them. The first finds the address on the default route —
/// the ordinary LAN address. The second is inside Tailscale's CGNAT range, so it
/// reveals the Tailscale address when that interface is up and is simply
/// unreachable-at-bind otherwise.
const ROUTE_PROBES: &[&str] = &["8.8.8.8:80", "100.64.0.1:80"];

/// Every place a probe should be sent, as `(bind address, broadcast address)`.
///
/// One entry per IPv4 interface, because a single datagram to 255.255.255.255
/// leaves by exactly one interface — whichever the routing table picks — and a
/// machine with more than one is common: a laptop on Wi-Fi and Ethernet at once,
/// or with a VPN up, will silently probe only one of its networks. Binding each
/// socket to an interface's own address is what forces the packet out of that
/// interface rather than the default one.
fn probe_targets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut targets = Vec::new();

    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }
            let if_addrs::IfAddr::V4(v4) = interface.addr else { continue };
            // A point-to-point interface such as Tailscale's has no broadcast
            // address at all, which is the honest reason discovery cannot reach
            // across it — not a limitation this code could work around.
            let Some(broadcast) = v4.broadcast else { continue };
            targets.push((v4.ip, broadcast));
        }
    }

    // Kept as a fallback for the case where enumeration failed or returned
    // nothing: the old behaviour is better than no behaviour.
    targets.push((Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST));
    targets
}

/// Whether something is accepting control connections at `host:port`.
///
/// Used to tell "no receiver" apart from "a receiver whose discovery replies are
/// being dropped" — a firewall that permits the control port while blocking the
/// discovery port leaves a receiver perfectly usable and completely unfindable,
/// and the advice for the two cases is opposite.
pub fn control_port_answers(host: &str, port: u16) -> bool {
    let Ok(addresses) = (host, port).to_socket_addrs() else { return false };
    addresses.into_iter().any(|address| {
        std::net::TcpStream::connect_timeout(&address, CONTROL_CHECK_TIMEOUT).is_ok()
    })
}

/// A receiver that answered a probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Address to give the sender as `--host`.
    pub host: IpAddr,
    /// Control port the receiver is listening on.
    pub control_port: u16,
    pub label: String,
}

impl Found {
    /// The form a user would type, and what the GUI puts in its host field.
    pub fn address(&self) -> String {
        format!("{}", self.host)
    }
}

/// Addresses this machine could plausibly be reached on by a peer.
///
/// Ordered and deduplicated, loopback and unspecified addresses removed — those are
/// never useful to type on the *other* machine, which is the whole point of showing
/// them.
pub fn local_addresses() -> Vec<IpAddr> {
    let mut found = BTreeSet::new();

    for target in ROUTE_PROBES {
        // A fresh socket per probe: binding to port 0 and connecting is enough to
        // make the kernel choose a source address, and costs nothing.
        let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { continue };
        if socket.connect(target).is_err() {
            continue;
        }
        if let Ok(local) = socket.local_addr() {
            let ip = local.ip();
            if !ip.is_loopback() && !ip.is_unspecified() {
                found.insert(ip);
            }
        }
    }

    found.into_iter().collect()
}

/// A running discovery responder. Dropping this does not stop it; the caller shares
/// the stop flag it was given.
pub struct Responder {
    pub thread: std::thread::JoinHandle<()>,
    /// The port actually bound, which may differ from the one requested.
    pub port: u16,
}

/// Answers probes with `control_port`, until `stop` is set.
///
/// Binding is best-effort by design: another receiver on the same machine may already
/// hold the discovery port, and that must not stop a session from running. The caller
/// gets an error only if it wants to report it, never as a reason to abort.
pub fn respond(
    control_port: u16,
    label: String,
    discovery_port: u16,
    stop: Arc<AtomicBool>,
) -> Result<Responder> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, discovery_port))
        .with_context(|| format!("binding discovery port {discovery_port}"))?;
    socket.set_read_timeout(Some(RESPONDER_POLL)).context("setting discovery read timeout")?;
    let port = socket.local_addr().context("reading discovery port")?.port();

    let thread = std::thread::Builder::new()
        .name("micbridge-discovery".into())
        .spawn(move || {
            let reply = match (Message::Announce { control_port, label }).encode() {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(%err, "could not build a discovery reply");
                    return;
                }
            };
            let mut buf = [0u8; 128];

            while !stop.load(Ordering::Relaxed) {
                let (len, from) = match socket.recv_from(&mut buf) {
                    Ok(pair) => pair,
                    Err(err) if crate::timing::is_timeout(&err) => continue,
                    // A broadcast responder sees whatever the segment throws at it;
                    // none of it is a reason to stop listening.
                    Err(err) => {
                        tracing::debug!(%err, "discovery receive failed");
                        continue;
                    }
                };

                match Message::decode(&buf[..len]) {
                    Ok(Message::Probe) => {
                        // Unicast the answer back rather than broadcasting it, so one
                        // curious host does not wake the whole segment.
                        if let Err(err) = socket.send_to(&reply, from) {
                            tracing::debug!(%err, %from, "could not answer a discovery probe");
                        } else {
                            tracing::debug!(%from, "answered a discovery probe");
                        }
                    }
                    // Another receiver announcing itself. Not our business.
                    Ok(Message::Announce { .. }) => {}
                    Err(err) => tracing::trace!(%err, "ignoring a foreign datagram"),
                }
            }
        })
        .context("spawning discovery responder")?;

    Ok(Responder { thread, port })
}

/// Broadcasts a probe and collects whoever answers.
///
/// Always returns after `PROBE_TIMEOUT` at the latest, and an empty list is a normal
/// outcome — broadcast is not guaranteed to go anywhere.
pub fn find(discovery_port: u16) -> Result<Vec<Found>> {
    let probe = Message::Probe.encode().context("encoding a probe")?;

    // One socket per interface. Non-blocking rather than each with a read timeout:
    // with several sockets, blocking reads would serialise and the whole probe
    // window could be spent waiting on the first one.
    let mut sockets = Vec::new();
    for (bind, broadcast) in probe_targets() {
        let Ok(socket) = UdpSocket::bind((bind, 0)) else { continue };
        if socket.set_broadcast(true).is_err() || socket.set_nonblocking(true).is_err() {
            continue;
        }
        sockets.push((socket, SocketAddr::from((broadcast, discovery_port))));
    }
    if sockets.is_empty() {
        anyhow::bail!("no usable network interface to probe from");
    }

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut sends_left = PROBE_REPEATS;
    let mut next_send = Instant::now();
    // Keyed so the same receiver answering on several interfaces appears once.
    let mut found: BTreeSet<(IpAddr, u16, String)> = BTreeSet::new();
    let mut buf = [0u8; 128];

    while Instant::now() < deadline {
        if sends_left > 0 && Instant::now() >= next_send {
            // A single unacknowledged broadcast can simply vanish, so send a few.
            for (socket, destination) in &sockets {
                if let Err(err) = socket.send_to(&probe, destination) {
                    tracing::debug!(%err, %destination, "broadcast probe failed");
                }
            }
            sends_left -= 1;
            next_send = Instant::now() + PROBE_TIMEOUT / (PROBE_REPEATS + 1);
        }

        let mut idle = true;
        for (socket, _) in &sockets {
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((len, from)) => {
                        idle = false;
                        if let Ok(Message::Announce { control_port, label }) =
                            Message::decode(&buf[..len])
                        {
                            found.insert((from.ip(), control_port, label));
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        tracing::debug!(%err, "probe receive failed");
                        break;
                    }
                }
            }
        }
        if idle {
            std::thread::sleep(PROBE_POLL);
        }
    }

    Ok(found
        .into_iter()
        .map(|(host, control_port, label)| Found { host, control_port, label })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this replaced: one datagram to 255.255.255.255 leaves by exactly one
    /// interface, so a machine on Wi-Fi and Ethernet at once probed only one of its
    /// networks and reported "nothing answered" with everything configured right.
    #[test]
    fn a_probe_is_addressed_to_every_interface() {
        let targets = probe_targets();
        assert!(!targets.is_empty(), "there is always the limited-broadcast fallback");

        let bindings: Vec<Ipv4Addr> = targets.iter().map(|(bind, _)| *bind).collect();
        assert!(
            bindings.contains(&Ipv4Addr::UNSPECIFIED),
            "the old behaviour must survive as a fallback: {targets:?}"
        );

        // Every entry is either the fallback or a real interface bound to its own
        // address — which is what forces the packet out of that interface.
        for (bind, broadcast) in &targets {
            if *bind == Ipv4Addr::UNSPECIFIED {
                assert_eq!(*broadcast, Ipv4Addr::BROADCAST);
            } else {
                assert!(!bind.is_loopback(), "loopback is not worth probing: {bind}");
                assert_ne!(bind, broadcast, "an interface must not probe its own address");
            }
        }
    }

    /// Duplicates would mean the same probe sent twice out of one interface.
    #[test]
    fn no_interface_is_probed_twice() {
        let targets = probe_targets();
        let unique: BTreeSet<_> = targets.iter().collect();
        assert_eq!(unique.len(), targets.len(), "duplicate probe targets: {targets:?}");
    }

    /// The check that tells "no receiver" apart from "receiver behind a firewall".
    /// A closed port must read as closed rather than hanging until a timeout that
    /// would make the Find button feel broken.
    #[test]
    fn a_closed_control_port_does_not_answer() {
        // Port 1 on loopback: nothing binds it, and the connection is refused
        // immediately rather than filtered.
        assert!(!control_port_answers("127.0.0.1", 1));
    }

    #[test]
    fn an_open_control_port_answers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(control_port_answers("127.0.0.1", port));
    }

    #[test]
    fn an_unresolvable_host_is_not_an_answer() {
        assert!(!control_port_answers("this-host-does-not-exist.invalid", 42100));
    }

    #[test]
    fn local_addresses_are_usable_by_a_peer() {
        // Cannot assert a specific address, but can assert the ones that would be
        // useless: telling the user to type 127.0.0.1 on the other machine is worse
        // than telling them nothing.
        for ip in local_addresses() {
            assert!(!ip.is_loopback(), "{ip} is loopback and cannot be reached from elsewhere");
            assert!(!ip.is_unspecified(), "{ip} is unspecified");
        }
    }

    #[test]
    fn local_addresses_are_deduplicated() {
        let addresses = local_addresses();
        let unique: BTreeSet<_> = addresses.iter().collect();
        assert_eq!(addresses.len(), unique.len(), "duplicates in {addresses:?}");
    }

    #[test]
    fn a_probe_finds_a_responder_on_this_machine() {
        // End to end over the loopback interface, on an ephemeral port so a real
        // receiver running on this machine cannot interfere.
        let stop = Arc::new(AtomicBool::new(false));
        let responder = respond(42_100, "test-receiver".into(), 0, Arc::clone(&stop))
            .expect("responder starts");
        let port = responder.port;
        assert_ne!(port, 0, "should report the port it bound");

        // Probe directly rather than by broadcast: CI runners and hardened networks
        // filter broadcast, and this test is about the exchange, not the routing.
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe socket");
        socket.set_read_timeout(Some(Duration::from_millis(500))).expect("timeout");
        socket
            .send_to(&Message::Probe.encode().expect("encodes"), (Ipv4Addr::LOCALHOST, port))
            .expect("probe sent");

        let mut buf = [0u8; 128];
        let (len, _) = socket.recv_from(&mut buf).expect("a reply should arrive");
        match Message::decode(&buf[..len]).expect("decodes") {
            Message::Announce { control_port, label } => {
                assert_eq!(control_port, 42_100, "must report the control port, not its own");
                assert_eq!(label, "test-receiver");
            }
            other => panic!("expected an announce, got {other:?}"),
        }

        stop.store(true, Ordering::Relaxed);
        responder.thread.join().expect("responder stops");
    }

    #[test]
    fn the_responder_stops_promptly_when_asked() {
        let stop = Arc::new(AtomicBool::new(false));
        let responder =
            respond(42_100, String::new(), 0, Arc::clone(&stop)).expect("responder starts");

        let asked = Instant::now();
        stop.store(true, Ordering::Relaxed);
        responder.thread.join().expect("responder stops");
        let took = asked.elapsed();

        // It blocks on a read with a timeout, so it should exit within about one poll.
        assert!(took < Duration::from_secs(2), "took {took:?} to stop");
    }

    #[test]
    fn garbage_does_not_kill_the_responder() {
        // A broadcast responder receives whatever is on the segment.
        let stop = Arc::new(AtomicBool::new(false));
        let responder =
            respond(42_100, String::new(), 0, Arc::clone(&stop)).expect("responder starts");
        let port = responder.port;

        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("socket");
        socket.set_read_timeout(Some(Duration::from_millis(500))).expect("timeout");
        for junk in [&b""[..], &b"hello"[..], &[0xFF; 64][..]] {
            socket.send_to(junk, (Ipv4Addr::LOCALHOST, port)).expect("junk sent");
        }

        // Still answering afterwards.
        socket
            .send_to(&Message::Probe.encode().expect("encodes"), (Ipv4Addr::LOCALHOST, port))
            .expect("probe sent");
        let mut buf = [0u8; 128];
        let (len, _) = socket.recv_from(&mut buf).expect("should still answer");
        assert!(matches!(Message::decode(&buf[..len]), Ok(Message::Announce { .. })));

        stop.store(true, Ordering::Relaxed);
        responder.thread.join().expect("responder stops");
    }

    #[test]
    fn find_returns_within_its_timeout_even_with_nobody_there() {
        // A "Find" button must not hang, and finding nothing is a normal answer.
        let started = Instant::now();
        let found = find(1).expect("probing should not error");
        let took = started.elapsed();
        assert!(found.is_empty(), "nothing listens on port 1: {found:?}");
        assert!(took < PROBE_TIMEOUT * 3, "took {took:?}");
    }
}
