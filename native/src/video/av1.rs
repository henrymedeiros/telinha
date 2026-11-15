//! AV1 utilities for the low-overhead OBU packets FFmpeg's encoders hand out:
//! the WebCodecs codec string from the sequence header, and the sequence header
//! on every keyframe (viewers who join mid-stream decode from it, and an
//! encoder may write it only once, at the start).
//!
//! The parser takes the packet as untrusted bytes: it never indexes past the
//! end and stops at the first OBU that does not fit.

use super::h264::Bits;

const SEQUENCE_HEADER: u8 = 1;
const TEMPORAL_DELIMITER: u8 = 2;

/// A LEB128 number at the start of `data`: (value, bytes it took). At most 8
/// bytes, as the AV1 spec allows; anything longer or cut short is `None`.
fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    for i in 0..8 {
        let byte = *data.get(i)?;
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, i + 1));
        }
    }
    None
}

/// Where an OBU's payload starts (after the header, the extension byte and the
/// size field) and, when it has a size field, how long the payload is.
fn payload(obu: &[u8]) -> Option<(usize, Option<usize>)> {
    let header = *obu.first()?;
    if header & 0x80 != 0 {
        return None; // obu_forbidden_bit
    }
    let mut start = if header & 0x04 != 0 { 2 } else { 1 };
    if start > obu.len() {
        return None;
    }
    if header & 0x02 == 0 {
        return Some((start, None));
    }
    let (size, n) = leb128(obu.get(start..)?)?;
    start += n;
    Some((start, Some(size)))
}

/// Each OBU in the packet as (obu_type, start, end), `start..end` covering the
/// header, the size field and the payload. Stops at the first one that does not fit.
fn obus(data: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let Some(rest) = data.get(pos..) else { break };
        let Some((start, size)) = payload(rest) else { break };
        let end = match size {
            Some(size) => match pos.checked_add(start).and_then(|s| s.checked_add(size)) {
                Some(end) if end <= data.len() => end,
                _ => break,
            },
            None => data.len(),
        };
        out.push(((rest[0] >> 3) & 0x0f, pos, end));
        // Every OBU takes at least its header byte, so this always moves forward.
        pos = end;
    }
    out
}

/// "av01.P.LLT.08" from a sequence header OBU (header byte and size field included).
pub fn codec_string(obu: &[u8]) -> Option<String> {
    let (start, size) = payload(obu)?;
    let rest = obu.get(start..)?;
    let body = match size {
        Some(size) if size < rest.len() => &rest[..size],
        _ => rest,
    };
    let mut r = Bits::new(body);
    let profile = r.bits(3)?; // seq_profile
    r.bits(1)?; // still_picture
    let (level, tier) = if r.bits(1)? == 1 {
        (r.bits(5)?, 0) // reduced_still_picture_header: one level, main tier
    } else {
        if r.bits(1)? == 1 {
            // timing_info
            r.bits(32)?; // num_units_in_display_tick
            r.bits(32)?; // time_scale
            if r.bits(1)? == 1 {
                // equal_picture_interval: num_ticks_per_picture_minus_1, uvlc
                let mut zeros = 0;
                while r.bits(1)? == 0 {
                    zeros += 1;
                    if zeros >= 32 {
                        return None;
                    }
                }
                r.bits(zeros)?;
            }
            if r.bits(1)? == 1 {
                // decoder_model_info
                r.bits(5)?; // buffer_delay_length_minus_1
                r.bits(32)?; // num_units_in_decoding_tick
                r.bits(5)?; // buffer_removal_time_length_minus_1
                r.bits(5)?; // frame_presentation_time_length_minus_1
            }
        }
        r.bits(1)?; // initial_display_delay_present_flag
        r.bits(5)?; // operating_points_cnt_minus_1
        r.bits(12)?; // operating_point_idc[0]
        let level = r.bits(5)?; // seq_level_idx[0]
        let tier = if level > 7 { r.bits(1)? } else { 0 };
        (level, tier)
    };
    // The app only feeds 8-bit pictures to the encoders, so the depth is always 08.
    Some(format!("av01.{profile}.{level:02}{}.08", if tier == 1 { 'H' } else { 'M' }))
}

/// Keeps the last sequence header and puts it into any keyframe that arrives without one.
#[derive(Default)]
pub struct Headers {
    seq: Option<Vec<u8>>,
    warned: bool,
}

impl Headers {
    pub fn codec(&self) -> Option<String> {
        self.seq.as_deref().and_then(codec_string)
    }

    pub fn fix_keyframe(&mut self, data: Vec<u8>, key: bool) -> Vec<u8> {
        let units = obus(&data);
        let mut has = false;
        for &(kind, start, end) in &units {
            if kind == SEQUENCE_HEADER {
                has = true;
                self.seq = Some(data[start..end].to_vec());
            }
        }
        if !key || has {
            return data;
        }
        let Some(seq) = &self.seq else {
            if !self.warned {
                self.warned = true;
                tracing::warn!("AV1 keyframe without a sequence header: viewers cannot start from it");
            }
            return data;
        };
        // The temporal delimiter, when there is one, must stay the first OBU of the packet.
        let split = match units.first() {
            Some(&(TEMPORAL_DELIMITER, _, end)) => end,
            _ => 0,
        };
        let mut out = Vec::with_capacity(data.len() + seq.len());
        out.extend_from_slice(&data[..split]);
        out.extend_from_slice(seq);
        out.extend_from_slice(&data[split..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sequence header OBUs from the bundled FFmpeg at 1280x720, 8-bit 4:2:0;
    // ffprobe reports both as Main profile, level index 8.
    // libsvtav1
    const SEQ_SVT: [u8; 14] = [0x0a, 0x0c, 0x02, 0x00, 0x00, 0x42, 0x95, 0x33, 0xfe, 0xcf, 0x1a, 0xbe, 0x60, 0x10];
    // libaom
    const SEQ_AOM: [u8; 13] = [0x0a, 0x0b, 0x00, 0x00, 0x00, 0x42, 0xa6, 0x7f, 0xd9, 0xef, 0xd7, 0xcc, 0x02];
    const DELIMITER: [u8; 2] = [0x12, 0x00];
    // An OBU of type 6 (frame) with a 3-byte payload.
    const FRAME: [u8; 5] = [0x32, 0x03, 0xaa, 0xbb, 0xcc];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn texto_do_codec() {
        assert_eq!(codec_string(&SEQ_SVT).as_deref(), Some("av01.0.08M.08"));
        assert_eq!(codec_string(&SEQ_AOM).as_deref(), Some("av01.0.08M.08"));
    }

    #[test]
    fn cabecalho_cortado_nao_quebra() {
        for n in 0..SEQ_SVT.len() {
            let s = codec_string(&SEQ_SVT[..n]);
            if n <= 5 {
                assert_eq!(s, None, "{n} bytes");
            }
        }
    }

    #[test]
    fn keyframe_sem_cabecalho_ganha_um() {
        let mut h = Headers::default();
        let first = cat(&[&DELIMITER, &SEQ_SVT, &FRAME]);
        assert_eq!(h.fix_keyframe(first.clone(), true), first);
        assert_eq!(h.codec().as_deref(), Some("av01.0.08M.08"));
        assert_eq!(h.fix_keyframe(cat(&[&DELIMITER, &FRAME]), true), cat(&[&DELIMITER, &SEQ_SVT, &FRAME]));
        assert_eq!(h.fix_keyframe(FRAME.to_vec(), true), cat(&[&SEQ_SVT, &FRAME]));
    }

    #[test]
    fn quadro_comum_passa_intacto() {
        let mut h = Headers::default();
        h.fix_keyframe(cat(&[&DELIMITER, &SEQ_SVT, &FRAME]), true);
        assert_eq!(h.fix_keyframe(FRAME.to_vec(), false), FRAME);
    }

    #[test]
    fn sem_cabecalho_conhecido_nao_inventa() {
        let mut h = Headers::default();
        assert_eq!(h.fix_keyframe(FRAME.to_vec(), true), FRAME);
        assert_eq!(h.codec(), None);
    }

    #[test]
    fn lixo_nao_quebra() {
        let junk: [&[u8]; 5] = [&[], &[0xff], &[0x0a], &[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], &[0x0a, 0x7f, 0x00]];
        for data in junk {
            for key in [true, false] {
                let mut h = Headers::default();
                assert_eq!(h.fix_keyframe(data.to_vec(), key), data, "{data:x?} key={key}");
            }
        }
    }
}
