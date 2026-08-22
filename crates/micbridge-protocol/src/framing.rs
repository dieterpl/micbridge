//! Length-prefixed framing for the control channel.
//!
//! Each frame is a 4-byte big-endian length followed by that many bytes of
//! MessagePack. Length-prefixing rather than a delimiter because a reader then
//! never has to guess where a message ends, and it is trivial to implement from
//! any language should a second client ever be written.
//!
//! # Why reads go through [`FrameReader`]
//!
//! The control socket has a read timeout, so the loop above it can send its own
//! heartbeats. That makes `read_exact` unusable: it consumes bytes into its buffer
//! and *then* returns the timeout error, so the bytes already taken off the socket
//! are lost. One timeout landing between a length prefix and its payload
//! permanently desynchronises the stream — the next read treats MessagePack
//! payload bytes as a length prefix and reports a two-gigabyte frame or a decode
//! error, and every frame after it is garbage.
//!
//! It needs a slow link to happen, not a broken one. Windows' minimum TCP
//! retransmit timeout is 300 ms against a 200 ms poll, so a single dropped segment
//! over Wi-Fi or Tailscale is enough.
//!
//! [`FrameReader`] therefore keeps partial progress across calls: a timeout is
//! reported as "no frame yet", and the next call resumes where it left off.

use std::io::{self, Read, Write};
use std::time::Instant;

use serde::{de::DeserializeOwned, Serialize};

/// Length of the frame's length prefix.
const PREFIX_LEN: usize = 4;

/// Maximum accepted frame length.
///
/// Control frames are a few hundred bytes; a megabyte is generous by three orders
/// of magnitude. The cap exists so a corrupt or hostile length prefix cannot make
/// the receiver allocate arbitrarily before it has read a byte of payload.
pub const MAX_FRAME_LEN: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("control channel I/O: {0}")]
    Io(#[from] io::Error),
    #[error("frame is {len} bytes, over the {MAX_FRAME_LEN}-byte limit")]
    FrameTooLarge { len: usize },
    #[error("encoding control message: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decoding control message: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("timed out waiting for a control message")]
    Timeout,
}

/// Serialises a message to MessagePack with named fields.
///
/// Named rather than positional so that field order in the Rust struct is not part
/// of the wire contract.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, Error> {
    Ok(rmp_serde::to_vec_named(msg)?)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    Ok(rmp_serde::from_slice(bytes)?)
}

/// Writes one frame and flushes it.
///
/// The flush matters: control messages are small and latency-sensitive, and a
/// buffered heartbeat that sits in userspace looks exactly like a dead peer.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> Result<(), Error> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge { len: payload.len() });
    }
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Encodes and writes a message in one call.
pub fn send<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), Error> {
    let payload = encode(msg)?;
    write_frame(w, &payload)
}

/// A read timeout surfaces as `WouldBlock` on Unix and `TimedOut` on Windows, and
/// both mean the same thing: nothing more has arrived yet.
fn is_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Which part of a frame is still outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prefix,
    Payload,
}

/// A resumable frame reader.
///
/// Holds one frame's worth of partial state so a read timeout costs nothing but a
/// return to the caller. One reader belongs to one socket for its whole life —
/// creating a fresh one per read would reintroduce exactly the bug it exists to
/// prevent.
#[derive(Debug)]
pub struct FrameReader {
    prefix: [u8; PREFIX_LEN],
    prefix_filled: usize,
    payload: Vec<u8>,
    payload_filled: usize,
    phase: Phase,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> Self {
        Self {
            prefix: [0; PREFIX_LEN],
            prefix_filled: 0,
            payload: Vec::new(),
            payload_filled: 0,
            phase: Phase::Prefix,
        }
    }

    /// True when part of a frame has been read but not all of it.
    ///
    /// Only useful for tests and diagnostics: a caller does not need to care,
    /// which is the point.
    pub fn is_mid_frame(&self) -> bool {
        self.prefix_filled > 0 || self.payload_filled > 0
    }

    /// Reads towards the next complete frame.
    ///
    /// Returns `Ok(Some(bytes))` for a complete frame, or `Ok(None)` if the read
    /// timed out first — in which case whatever arrived is retained and the next
    /// call continues from there.
    pub fn poll<'a, R: Read>(&'a mut self, r: &mut R) -> Result<Option<&'a [u8]>, Error> {
        loop {
            match self.phase {
                Phase::Prefix => {
                    while self.prefix_filled < PREFIX_LEN {
                        match r.read(&mut self.prefix[self.prefix_filled..]) {
                            Ok(0) => {
                                return Err(Error::Io(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "peer closed the control connection",
                                )))
                            }
                            Ok(n) => self.prefix_filled += n,
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                            Err(err) if is_timeout(&err) => return Ok(None),
                            Err(err) => return Err(Error::Io(err)),
                        }
                    }

                    let len = u32::from_be_bytes(self.prefix) as usize;
                    if len > MAX_FRAME_LEN {
                        return Err(Error::FrameTooLarge { len });
                    }
                    // Sized up front so the payload phase reads straight into it.
                    self.payload.clear();
                    self.payload.resize(len, 0);
                    self.payload_filled = 0;
                    self.phase = Phase::Payload;
                }
                Phase::Payload => {
                    while self.payload_filled < self.payload.len() {
                        match r.read(&mut self.payload[self.payload_filled..]) {
                            Ok(0) => {
                                return Err(Error::Io(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "peer closed mid-frame",
                                )))
                            }
                            Ok(n) => self.payload_filled += n,
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                            Err(err) if is_timeout(&err) => return Ok(None),
                            Err(err) => return Err(Error::Io(err)),
                        }
                    }

                    // Ready. Reset for the next frame before handing this one out.
                    self.phase = Phase::Prefix;
                    self.prefix_filled = 0;
                    let len = self.payload_filled;
                    self.payload_filled = 0;
                    return Ok(Some(&self.payload[..len]));
                }
            }
        }
    }

    /// [`FrameReader::poll`] plus decoding.
    pub fn poll_msg<R: Read, T: DeserializeOwned>(
        &mut self,
        r: &mut R,
    ) -> Result<Option<T>, Error> {
        match self.poll(r)? {
            Some(bytes) => Ok(Some(decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Polls until a frame arrives or `deadline` passes.
    ///
    /// For the handshake, where there is nothing else to do until the peer speaks.
    pub fn recv_until<R: Read, T: DeserializeOwned>(
        &mut self,
        r: &mut R,
        deadline: Instant,
    ) -> Result<T, Error> {
        loop {
            if let Some(msg) = self.poll_msg(r)? {
                return Ok(msg);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ClientMessage, Hello, StreamFormat, WireSampleFormat};

    fn hello() -> ClientMessage {
        ClientMessage::Hello(Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            session_id: 7,
            device_name: "test".to_string(),
            format: StreamFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: WireSampleFormat::S16Le,
                frames_per_packet: 240,
            },
        })
    }

    /// A reader that hands out at most `chunk` bytes per call and then reports a
    /// timeout, modelling a slow or lossy link.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        /// Report a timeout before every read once this is set, alternating, so
        /// every boundary in the frame gets exercised.
        timeout_next: bool,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.timeout_next {
                self.timeout_next = false;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "simulated timeout"));
            }
            self.timeout_next = true;
            if self.pos >= self.data.len() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing left"));
            }
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn length_prefix_is_four_bytes_big_endian() {
        let mut stream = Vec::new();
        write_frame(&mut stream, &[0xAA, 0xBB, 0xCC]).expect("writes");
        assert_eq!(&stream, &[0, 0, 0, 3, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn frame_round_trips_through_a_byte_stream() {
        let mut stream = Vec::new();
        send(&mut stream, &hello()).expect("sends");

        let mut cursor = io::Cursor::new(stream);
        let mut reader = FrameReader::new();
        let decoded: ClientMessage =
            reader.poll_msg(&mut cursor).expect("reads").expect("a whole frame");
        assert_eq!(decoded, hello());
    }

    #[test]
    fn back_to_back_frames_are_read_independently() {
        let mut stream = Vec::new();
        send(&mut stream, &ClientMessage::Heartbeat).expect("sends");
        send(&mut stream, &ClientMessage::Goodbye).expect("sends");

        let mut cursor = io::Cursor::new(stream);
        let mut reader = FrameReader::new();
        assert_eq!(
            reader.poll_msg::<_, ClientMessage>(&mut cursor).expect("first").expect("frame"),
            ClientMessage::Heartbeat
        );
        assert_eq!(
            reader.poll_msg::<_, ClientMessage>(&mut cursor).expect("second").expect("frame"),
            ClientMessage::Goodbye
        );
    }

    #[test]
    fn a_timeout_mid_frame_does_not_desynchronise_the_stream() {
        // The bug this module exists to prevent. One byte at a time with a timeout
        // between every read puts a timeout at every boundary in the frame,
        // including between the length prefix and the payload.
        let mut stream = Vec::new();
        send(&mut stream, &hello()).expect("sends");
        send(&mut stream, &ClientMessage::Goodbye).expect("sends");

        let mut trickle = Trickle { data: stream, pos: 0, chunk: 1, timeout_next: true };
        let mut reader = FrameReader::new();

        let mut received: Vec<ClientMessage> = Vec::new();
        // Generously more polls than bytes; each poll makes at most one byte of
        // progress.
        for _ in 0..10_000 {
            match reader.poll_msg::<_, ClientMessage>(&mut trickle) {
                Ok(Some(msg)) => received.push(msg),
                Ok(None) => {}
                Err(err) => panic!("stream desynchronised: {err}"),
            }
            if received.len() == 2 {
                break;
            }
        }

        assert_eq!(received, vec![hello(), ClientMessage::Goodbye]);
    }

    #[test]
    fn partial_progress_is_visible_and_then_cleared() {
        let mut stream = Vec::new();
        send(&mut stream, &hello()).expect("sends");

        // Two bytes: not even the whole length prefix.
        let mut trickle = Trickle { data: stream, pos: 0, chunk: 2, timeout_next: false };
        let mut reader = FrameReader::new();
        assert!(reader.poll_msg::<_, ClientMessage>(&mut trickle).expect("no error").is_none());
        assert!(reader.is_mid_frame(), "should be holding a partial prefix");

        for _ in 0..10_000 {
            if reader.poll_msg::<_, ClientMessage>(&mut trickle).expect("no error").is_some() {
                break;
            }
        }
        assert!(!reader.is_mid_frame(), "state should reset once a frame completes");
    }

    #[test]
    fn oversized_length_prefix_is_rejected_before_allocating() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());
        let mut cursor = io::Cursor::new(stream);
        let mut reader = FrameReader::new();
        assert!(matches!(reader.poll(&mut cursor), Err(Error::FrameTooLarge { .. })));
    }

    #[test]
    fn clean_close_between_frames_surfaces_as_eof() {
        let mut cursor = io::Cursor::new(Vec::new());
        let mut reader = FrameReader::new();
        match reader.poll(&mut cursor) {
            Err(Error::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected EOF, got {other:?}"),
        }
    }

    #[test]
    fn a_close_mid_frame_is_eof_not_a_silent_short_frame() {
        // Prefix says four bytes, only two arrive, then the peer closes. Returning
        // a two-byte frame would hand a truncated message to the decoder.
        let stream = vec![0, 0, 0, 4, 0xAA, 0xBB];
        let mut cursor = io::Cursor::new(stream);
        let mut reader = FrameReader::new();
        match reader.poll(&mut cursor) {
            Err(Error::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected EOF, got {other:?}"),
        }
    }

    #[test]
    fn empty_frame_is_legal() {
        let mut stream = Vec::new();
        write_frame(&mut stream, &[]).expect("writes");
        let mut cursor = io::Cursor::new(stream);
        let mut reader = FrameReader::new();
        assert_eq!(reader.poll(&mut cursor).expect("reads"), Some(&[][..]));
    }

    #[test]
    fn recv_until_gives_up_at_its_deadline_rather_than_hanging() {
        struct Silent;
        impl Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::TimedOut, "silence"))
            }
        }

        let mut reader = FrameReader::new();
        let deadline = Instant::now() + std::time::Duration::from_millis(50);
        let result: Result<ClientMessage, Error> = reader.recv_until(&mut Silent, deadline);
        assert!(matches!(result, Err(Error::Timeout)), "should time out, got {result:?}");
    }

    #[test]
    fn windows_timeout_kind_is_treated_as_a_timeout_too() {
        // On Windows a read timeout is TimedOut, not WouldBlock. Getting this wrong
        // turns every idle poll into a fatal error on one platform only.
        assert!(is_timeout(&io::Error::from(io::ErrorKind::TimedOut)));
        assert!(is_timeout(&io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(!is_timeout(&io::Error::from(io::ErrorKind::ConnectionReset)));
    }
}
