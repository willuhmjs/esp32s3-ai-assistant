// Minimal WAV (RIFF/PCM) writer for 16-bit mono audio.
//
// Write-only: the recorder wraps its PCM in a header for the STT upload, and
// that is the only WAV this firmware handles. The reader half - `parse_fmt`,
// `data_chunk_len`, `find_data_chunk` - was deleted when `speak()` moved to
// mp3 (see `TTS_RESPONSE_FORMAT`), which left it with no callers.

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
