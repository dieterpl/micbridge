//! The discovery datagram, so a sender can find a receiver without being told an
//! address.
//!
//! Deliberately separate from the control and media protocols and deliberately
//! tiny: a probe is broadcast to a whole network segment by a machine that knows
//! nothing yet, so it must be cheap to parse and impossible to confuse with audio.
//!
//! Broadcast only reaches a local segment. It does **not** traverse Tailscale, a
//! VPN, or a routed subnet, and some Wi-Fi access points filter it. So discovery is
//! a convenience, never the only way in — a receiver also reports the addresses it
//! can be reached on, which works everywhere.

/// `"RD"` — micbridge discovery.
///
/// Deliberately **not** the media channel's `"RP"`. Sharing it was the first thing
/// tried, and it collided: a media header's magic and version are identical, and its
/// `channel = 0` (audio) is byte-for-byte the same as `kind = 0` (probe), so an audio
/// datagram parsed cleanly as a valid probe. The two protocols live on different
/// ports, so it would rarely bite — but "rarely" is how a receiver ends up answering
/// a stream of audio with discovery replies.
pub const MAGIC: u16 = 0x5244;

/// Discovery port. Adjacent to the control (42100) and media (42101) ports.
pub const DEFAULT_DISCOVERY_PORT: u16 = 42102;

/// Version of the discovery exchange, bumped independently of the session
/// protocol — an old sender should still be able to find a new receiver.
pub const DISCOVERY_VERSION: u8 = 1;

const KIND_PROBE: u8 = 0;
const KIND_ANNOUNCE: u8 = 1;

/// Longest label a receiver may report, so a probe reply cannot be inflated.
pub const MAX_LABEL_LEN: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("datagram too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic {0:#06x}")]
    BadMagic(u16),
    #[error("unsupported discovery version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown discovery kind {0}")]
    UnknownKind(u8),
    #[error("label is {0} bytes, over the {MAX_LABEL_LEN}-byte limit")]
    LabelTooLong(usize),
    #[error("label is not valid UTF-8")]
    LabelNotUtf8,
}

/// A discovery datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// "Is anyone listening?" Sent by a would-be sender to a broadcast address.
    Probe,
    /// "Yes, here, on this control port." Sent by a receiver, unicast back to the
    /// prober.
    Announce {
        /// The control port to connect to — not the port this reply came from, since
        /// the two are deliberately different.
        control_port: u16,
        /// Something human-readable to tell two receivers apart. May be empty.
        label: String,
    },
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::with_capacity(8 + MAX_LABEL_LEN);
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.push(DISCOVERY_VERSION);

        match self {
            Self::Probe => out.push(KIND_PROBE),
            Self::Announce { control_port, label } => {
                let bytes = label.as_bytes();
                if bytes.len() > MAX_LABEL_LEN {
                    return Err(Error::LabelTooLong(bytes.len()));
                }
                out.push(KIND_ANNOUNCE);
                out.extend_from_slice(&control_port.to_be_bytes());
                out.push(bytes.len() as u8);
                out.extend_from_slice(bytes);
            }
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 4 {
            return Err(Error::TooShort(bytes.len()));
        }
        let magic = u16::from_be_bytes([bytes[0], bytes[1]]);
        if magic != MAGIC {
            return Err(Error::BadMagic(magic));
        }
        if bytes[2] != DISCOVERY_VERSION {
            return Err(Error::UnsupportedVersion(bytes[2]));
        }

        match bytes[3] {
            KIND_PROBE => Ok(Self::Probe),
            KIND_ANNOUNCE => {
                // magic(2) version(1) kind(1) port(2) label_len(1) = 7
                if bytes.len() < 7 {
                    return Err(Error::TooShort(bytes.len()));
                }
                let control_port = u16::from_be_bytes([bytes[4], bytes[5]]);
                let label_len = bytes[6] as usize;
                if label_len > MAX_LABEL_LEN {
                    return Err(Error::LabelTooLong(label_len));
                }
                if bytes.len() < 7 + label_len {
                    return Err(Error::TooShort(bytes.len()));
                }
                let label = std::str::from_utf8(&bytes[7..7 + label_len])
                    .map_err(|_| Error::LabelNotUtf8)?
                    .to_string();
                Ok(Self::Announce { control_port, label })
            }
            other => Err(Error::UnknownKind(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_round_trips() {
        let bytes = Message::Probe.encode().expect("encodes");
        assert_eq!(Message::decode(&bytes).expect("decodes"), Message::Probe);
    }

    #[test]
    fn announce_round_trips() {
        let msg = Message::Announce { control_port: 42_100, label: "windows-box".into() };
        let bytes = msg.encode().expect("encodes");
        assert_eq!(Message::decode(&bytes).expect("decodes"), msg);
    }

    #[test]
    fn an_empty_label_is_legal() {
        let msg = Message::Announce { control_port: 1, label: String::new() };
        let bytes = msg.encode().expect("encodes");
        assert_eq!(Message::decode(&bytes).expect("decodes"), msg);
    }

    #[test]
    fn a_probe_is_tiny() {
        // It goes to every host on the segment, so size is not incidental.
        assert_eq!(Message::Probe.encode().expect("encodes").len(), 4);
    }

    #[test]
    fn rejects_rubbish_rather_than_guessing() {
        assert_eq!(Message::decode(&[]), Err(Error::TooShort(0)));
        assert_eq!(Message::decode(&[0xFF, 0xFF, 1, 0]), Err(Error::BadMagic(0xFFFF)));

        let mut bad = Message::Probe.encode().expect("encodes");
        bad[2] = 99;
        assert_eq!(Message::decode(&bad), Err(Error::UnsupportedVersion(99)));

        let mut bad = Message::Probe.encode().expect("encodes");
        bad[3] = 42;
        assert_eq!(Message::decode(&bad), Err(Error::UnknownKind(42)));
    }

    #[test]
    fn a_truncated_announce_is_rejected_not_partly_believed() {
        // A short read must never yield a plausible-looking port.
        let full = Message::Announce { control_port: 42_100, label: "abc".into() }
            .encode()
            .expect("encodes");
        for len in 4..full.len() {
            assert!(
                Message::decode(&full[..len]).is_err(),
                "{len} bytes should be rejected, not parsed"
            );
        }
    }

    #[test]
    fn an_oversized_label_is_refused_on_both_sides() {
        let long = "x".repeat(MAX_LABEL_LEN + 1);
        assert!(matches!(
            Message::Announce { control_port: 1, label: long }.encode(),
            Err(Error::LabelTooLong(_))
        ));

        // And a hostile length byte cannot make a decoder over-read. Built from the
        // constants rather than literals, so changing the magic cannot quietly turn
        // this into a bad-magic test that passes for the wrong reason.
        let mut forged = Vec::new();
        forged.extend_from_slice(&MAGIC.to_be_bytes());
        forged.push(DISCOVERY_VERSION);
        forged.push(KIND_ANNOUNCE);
        forged.extend_from_slice(&1u16.to_be_bytes());
        forged.push(255);
        assert!(matches!(Message::decode(&forged), Err(Error::LabelTooLong(255))));
    }

    #[test]
    fn a_media_datagram_is_not_mistaken_for_discovery() {
        // The collision this magic exists to avoid: with the media magic, an audio
        // header's channel byte (0 = audio) is indistinguishable from a probe's kind
        // byte (0 = probe), and every audio datagram parsed as a valid probe.
        assert_ne!(MAGIC, crate::MAGIC, "discovery must not share the media magic");

        for channel in [crate::CHANNEL_AUDIO, crate::CHANNEL_HID] {
            let media = crate::MediaHeader::new(channel, 7, 1_000).encode();
            assert!(
                Message::decode(&media).is_err(),
                "must not parse a media header on channel {channel}"
            );
        }
    }

    #[test]
    fn a_discovery_datagram_is_not_mistaken_for_media() {
        // And the reverse, so a probe arriving on the media port is discarded.
        let probe = Message::Probe.encode().expect("encodes");
        assert!(crate::MediaHeader::decode(&probe).is_err(), "must not parse as media");
    }
}
