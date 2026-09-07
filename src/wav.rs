// Minimal WAV (RIFF/PCM) helpers for 16-bit mono audio.

extern crate alloc;
use alloc::vec::Vec;

pub fn build_wav_header(pcm_len: usize, sample_rate: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let byte_rate = sample_rate * 2; // mono, 16-bit
    let block_align: u16 = 2;
    let bits_per_sample: u16 = 16;
    let riff_len = (36 + pcm_len) as u32;

    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff_len.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&(pcm_len as u32).to_le_bytes());
    h
}

pub fn wav_bytes(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let header = build_wav_header(pcm.len(), sample_rate);
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(pcm);
    out
}

/// The `fmt ` chunk's contents: (format tag, channels, sample rate, bits per
/// sample). Format tag 1 is integer PCM; 3 is IEEE float, which this firmware
/// cannot play.
///
/// Nothing depends on this - it exists so the log can say what the server
/// actually sent, rather than the firmware assuming 24kHz mono 16-bit and
/// producing distortion if that assumption is ever wrong.
pub fn parse_fmt(wav: &[u8]) -> Option<(u16, u16, u32, u16)> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let chunk_id = &wav[pos..pos + 4];
        let chunk_len =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        if chunk_id == b"fmt " && pos + 8 + 16 <= wav.len() {
            let b = &wav[pos + 8..];
            return Some((
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
                u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                u16::from_le_bytes([b[14], b[15]]),
            ));
        }
        pos += 8 + chunk_len + (chunk_len % 2);
    }
    None
}

/// The `data` chunk's declared payload length, if the header states a
/// believable one.
///
/// A server that streams its WAV out as it synthesises doesn't know the length
/// when it writes the header, and writes a placeholder instead - 0, or
/// 0xFFFFFFFF, or the maximum RIFF size. Those are rejected here so the caller
/// can fall back to Content-Length rather than trusting a made-up number.
pub fn data_chunk_len(wav: &[u8]) -> Option<usize> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let chunk_id = &wav[pos..pos + 4];
        let chunk_len =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]);
        if chunk_id == b"data" {
            // 100 MB of 24kHz mono is half an hour of speech; anything at or
            // past that is a placeholder, not a length.
            if chunk_len == 0 || chunk_len as usize > 100 * 1024 * 1024 {
                return None;
            }
            return Some(chunk_len as usize);
        }
        pos += 8 + chunk_len as usize + (chunk_len as usize % 2);
    }
    None
}

/// Finds the start of the `data` chunk's payload in a WAV file, skipping
/// past any header/other chunks. Returns the offset, or 0 if not found
/// (caller can fall back to skipping the standard 44-byte header).
pub fn find_data_chunk(wav: &[u8]) -> usize {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return 44.min(wav.len());
    }
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let chunk_id = &wav[pos..pos + 4];
        let chunk_len = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        if chunk_id == b"data" {
            return pos + 8;
        }
        pos += 8 + chunk_len + (chunk_len % 2);
    }
    44.min(wav.len())
}
