//! Conversions between the `f32` samples the audio callbacks deal in and the
//! `i16` samples on the wire.

/// Scales by 32768 rather than 32767, so a sample that originated as an `i16`
/// survives the round trip bit-exactly and full-scale negative does not clip.
/// The clamp is what keeps `+1.0` — which scales to 32768, one past the top of
/// the range — from wrapping to full-scale negative.
#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    (sample * 32_768.0).clamp(-32_768.0, 32_767.0) as i16
}

#[inline]
pub fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32_768.0
}

/// Encodes interleaved `f32` into an interleaved `i16` buffer, replacing its
/// contents.
pub fn encode_into(samples: &[f32], out: &mut Vec<i16>) {
    out.clear();
    out.extend(samples.iter().copied().map(f32_to_i16));
}

/// Reinterprets the little-endian bytes of a media payload as `i16` samples.
///
/// Returns `None` on an odd length: a truncated frame would silently shift the
/// channel interleave, so it is rejected rather than partially accepted.
pub fn payload_to_i16(bytes: &[u8], out: &mut Vec<i16>) -> Option<()> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    out.clear();
    out.extend(bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
    Some(())
}

/// Appends `i16` samples to a byte buffer as little-endian pairs.
pub fn i16_to_payload(samples: &[i16], out: &mut Vec<u8>) {
    out.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_i16_exactly() {
        for s in [i16::MIN, -32_767, -1, 0, 1, 16_384, i16::MAX] {
            assert_eq!(f32_to_i16(i16_to_f32(s)), s, "sample {s}");
        }
    }

    #[test]
    fn clamps_instead_of_wrapping_at_full_scale() {
        // The bug this guards: `+1.0 * 32768` is 32768, which wraps to -32768
        // without the clamp. A single wrapped sample is a loud click.
        assert_eq!(f32_to_i16(1.0), 32_767);
        assert_eq!(f32_to_i16(2.5), 32_767);
        assert_eq!(f32_to_i16(-1.0), -32_768);
        assert_eq!(f32_to_i16(-4.0), -32_768);
    }

    #[test]
    fn payload_round_trips() {
        let samples = vec![0i16, 1, -1, i16::MAX, i16::MIN, 1234];
        let mut bytes = Vec::new();
        i16_to_payload(&samples, &mut bytes);
        assert_eq!(bytes.len(), samples.len() * 2);

        let mut decoded = Vec::new();
        payload_to_i16(&bytes, &mut decoded).expect("even length");
        assert_eq!(decoded, samples);
    }

    #[test]
    fn odd_length_payload_is_rejected() {
        let mut decoded = vec![7i16];
        assert!(payload_to_i16(&[1, 2, 3], &mut decoded).is_none());
    }

    #[test]
    fn encode_into_replaces_rather_than_appends() {
        let mut out = vec![9i16; 5];
        encode_into(&[0.0, 0.5], &mut out);
        assert_eq!(out, vec![0, 16_384]);
    }
}
