//! Making the window photograph itself.
//!
//! Only compiled with `--features screenshot`, so nothing here reaches a release
//! binary. It exists because the window could not otherwise be *looked at*: taking
//! a screenshot of another application needs Screen Recording permission, which is
//! not always available — and a UI nobody can see is a UI being written blind.
//!
//! egui renders the frame and hands it back through `Event::Screenshot`, so this
//! needs no permission of any kind and works on a machine with no display server.
//!
//!     cargo run -p micbridge-gui --features screenshot -- --screenshot shot.png
//!
//! What it photographs is live state, so a picture of a real session is also a
//! picture of the machine that took it — see `redact` below, which substitutes the
//! documentation values before the shot rather than after it.
//!
//! The PNG encoder below writes *stored* deflate blocks — the uncompressed form the
//! zlib format allows. That keeps this dependency-free at the cost of a large file,
//! which is the right trade for something that only ever runs on a developer's
//! machine and never ships.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use eframe::egui;
use micbridge_engine::Snapshot;

/// Frames to draw before the shot is taken.
///
/// Not one: the first frame lays out at a default size and egui's own state — combo
/// widths, wrapped rows — settles over the next few. Photographing frame one
/// produces a picture of a window mid-assembly.
pub const WARMUP_FRAMES: u32 = 8;

/// `MICBRIDGE_SCREENSHOT_FRAMES` overrides the warm-up.
///
/// Needed to photograph a *running* session: the shot has to wait until a sender
/// has connected and audio is actually moving, which is several seconds rather
/// than several frames.
pub fn warmup_frames() -> u32 {
    std::env::var("MICBRIDGE_SCREENSHOT_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(WARMUP_FRAMES)
}

/// Writes a `ColorImage` out as a PNG.
pub fn save(image: &egui::ColorImage, path: &Path) -> std::io::Result<()> {
    let (width, height) = (image.size[0], image.size[1]);

    let mut raw = Vec::with_capacity(height * (1 + width * 4));
    for y in 0..height {
        raw.push(0); // PNG filter: none
        for x in 0..width {
            let px = image.pixels[y * width + x];
            raw.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut header = Vec::new();
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);

    std::fs::File::create(path)?.write_all(&out)
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&out[start..]).to_be_bytes());
}

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32K window, no preset dictionary
    let mut chunks = data.chunks(0xFFFF).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(block) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(u8::from(last));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// ── Redaction ────────────────────────────────────────────────────────────────
//
// A capture headed for `docs/images/` must not photograph the machine that took
// it. Two things on screen come from the machine rather than from the program:
// the "send to" banner, which carries the addresses a peer could reach this
// receiver on — a LAN address, and a Tailscale one whenever that interface is up
// — and the device lists, which carry whatever happens to be plugged in. Both
// reached the repository once already.
//
// The substitution therefore lives on the capture path rather than in an image
// editor: painting over a PNG fixes one file and leaves the next capture to
// reintroduce the same values.

/// What the "send to" banner shows in a capture.
///
/// The documentation values, matching the examples in `README.md` and
/// `docs/testing.md`: an RFC 1918 address and one from the CGNAT range Tailscale
/// hands out.
const DOC_REACHABLE: [&str; 2] = ["192.168.1.20:42100", "100.64.0.5:42100"];

/// What any other address in a capture is reported as.
const DOC_ADDR: &str = "192.168.1.20";

/// Device names the documentation already uses, and which therefore identify a
/// product rather than a person.
const DOCUMENTED_DEVICES: [&str; 5] =
    ["UMC204HD", "CABLE Input", "CABLE Output", "MicBridge_Input", "MicBridge_Output"];

/// Stand-in for a device a session is running on that the documentation does not
/// name. Only reachable through the CLI's device flags, since the window's own
/// lists are filtered before they are drawn.
const GENERIC_DEVICE: &str = "Audio Interface";

/// Replaces everything in a snapshot that identifies the machine it came from.
pub fn redact(snapshot: &mut Snapshot) {
    snapshot.stats.reachable = DOC_REACHABLE.iter().map(|addr| (*addr).to_string()).collect();
    snapshot.stats.endpoint = documented_device(&snapshot.stats.endpoint);
    snapshot.stats.game_device = documented_device(&snapshot.stats.game_device);
    for line in &mut snapshot.log {
        *line = scrub_addrs(line);
    }
}

/// Drops undocumented devices from an enumerated list.
///
/// Dropped rather than renamed, deliberately: a filtered list still offers only
/// devices that exist, where an invented name would be one the session could not
/// open when the Start button was pressed on it. A capture run on a machine with
/// none of these attached shows an empty dropdown, which is the honest picture.
pub fn retain_documented_devices(names: &mut Vec<String>) {
    names.retain(|name| is_documented(name));
}

fn is_documented(name: &str) -> bool {
    DOCUMENTED_DEVICES.iter().any(|known| name.contains(known))
}

fn documented_device(name: &str) -> String {
    if name.is_empty() || is_documented(name) {
        name.to_string()
    } else {
        GENERIC_DEVICE.to_string()
    }
}

/// Rewrites every address in a line of log text that is not loopback.
///
/// Hand-rolled rather than pulled from a regex crate, for the same reason the PNG
/// writer above is: this module is compiled only on a developer's machine, and
/// adding a dependency to the workspace to serve it would be the wrong trade.
///
/// Loopback is left alone. `sender connected from 127.0.0.1:63621` says something
/// true about the loopback test that produced the screenshot and identifies
/// nobody.
fn scrub_addrs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();

    for ch in text.chars() {
        // The character set an IPv4 address, an IPv6 address, and a `host:port`
        // pair are all spelled from. Anything else ends the run.
        if ch.is_ascii_hexdigit() || ch == '.' || ch == ':' {
            token.push(ch);
            continue;
        }
        out.push_str(&scrub_token(&token));
        token.clear();
        out.push(ch);
    }
    out.push_str(&scrub_token(&token));
    out
}

/// One run of address-shaped characters, replaced only if it really parses as an
/// address. Everything else — `48000`, `fill_ms=19.4`, a crest factor of `1.41` —
/// fails to parse and comes back untouched.
fn scrub_token(token: &str) -> String {
    if let Ok(addr) = token.parse::<SocketAddr>() {
        if identifies_a_machine(addr.ip()) {
            return format!("{DOC_ADDR}:{}", addr.port());
        }
    } else if let Ok(ip) = token.parse::<IpAddr>() {
        if identifies_a_machine(ip) {
            return DOC_ADDR.to_string();
        }
    }
    token.to_string()
}

fn identifies_a_machine(ip: IpAddr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two checksums are the whole risk in a hand-rolled encoder: everything
    /// else is layout, and a wrong CRC makes a file no decoder will open.
    #[test]
    fn the_checksums_match_their_reference_values() {
        // "123456789" is the standard vector for both algorithms.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"123456789"), 0x091E_01DE);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn a_written_png_has_the_right_shape() {
        let image = egui::ColorImage::new([3, 2], vec![egui::Color32::from_rgb(1, 2, 3); 6]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.png");
        save(&image, &path).expect("write");

        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "missing signature");
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 3, "width");
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 2, "height");
        assert!(bytes.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "missing IEND");
    }

    /// A stored block cannot exceed 65535 bytes, so anything larger has to be split
    /// — and only the final block may carry the last-block flag.
    #[test]
    fn large_data_is_split_across_blocks() {
        let data = vec![7u8; 0xFFFF * 2 + 10];
        let stream = zlib_stored(&data);
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        assert_eq!(stream[2], 0, "the first of three blocks is not the last");
        assert_eq!(stream.len(), 2 + 3 * 5 + data.len() + 4);
    }

    /// What a screenshot of a running receiver would otherwise carry off the
    /// machine that took it: two reachable addresses and a log naming a third.
    ///
    /// The addresses standing in for the real ones are RFC 5737 documentation
    /// space and an arbitrary CGNAT address — a test for not publishing someone's
    /// network should not itself publish one.
    #[test]
    fn a_redacted_snapshot_carries_documentation_addresses_only() {
        use micbridge_engine::{Stats, Status};

        let mut snapshot = Snapshot {
            status: Status::Running,
            stats: Stats {
                reachable: vec!["100.90.80.70:42100".into(), "198.51.100.23:42100".into()],
                endpoint: "UMC204HD 192k".into(),
                game_device: "UMC204HD 192k".into(),
                ..Stats::default()
            },
            level: 0.5,
            log: vec![
                "sender connected from 127.0.0.1:63621".into(),
                "announcing on 198.51.100.23".into(),
            ],
            elapsed_secs: 12.0,
        };

        redact(&mut snapshot);

        assert_eq!(snapshot.stats.reachable, DOC_REACHABLE);
        assert_eq!(
            snapshot.log[0], "sender connected from 127.0.0.1:63621",
            "loopback identifies nobody and says something true about the test"
        );
        assert_eq!(snapshot.log[1], "announcing on 192.168.1.20");
        assert_eq!(snapshot.stats.endpoint, "UMC204HD 192k", "a documented device is left alone");
    }

    /// The log is mostly numbers that are not addresses, and a scrubber that
    /// mangled `fill_ms=19.4` would make the picture lie about the program.
    #[test]
    fn scrubbing_leaves_everything_that_is_not_an_address() {
        let line = "playing fill_ms=19.4 trim_ppm=-485 packets=201 rate 48000 crest 1.41";
        assert_eq!(scrub_addrs(line), line);
    }

    #[test]
    fn undocumented_devices_are_dropped_rather_than_renamed() {
        let mut names = vec!["MacBook Pro Microphone".to_string(), "UMC204HD 192k".to_string()];
        retain_documented_devices(&mut names);
        assert_eq!(names, ["UMC204HD 192k"]);
    }

    /// A session started from the CLI can be running on a device the window would
    /// have filtered out. It still must not be named.
    #[test]
    fn an_undocumented_running_device_shows_as_generic() {
        assert_eq!(documented_device("Scarlett 2i2 USB"), GENERIC_DEVICE);
        assert_eq!(documented_device(""), "", "an idle session names no device at all");
    }
}
