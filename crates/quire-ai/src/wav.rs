// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WAV files for voice notes: written by the app (16-bit PCM, mono) and
//! read by the engine for transcription (16-, 24- or 32-bit PCM or 32-bit
//! float, any channel count, mixed down to mono).
//!
//! Voice notes are kept as WAV because no pure-Rust Opus encoder is mature
//! enough to ship; 16 kHz mono is 32 KB a second (about 1.9 MB a minute).

/// The sample rate voice notes are recorded at (what Whisper expects).
pub const VOICE_RATE: u32 = 16_000;

/// Mono 16-bit PCM WAV bytes of `samples` (−1.0..1.0) at `rate`.
pub fn encode_pcm16(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX - 36);
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Sample rates a WAV file may claim (0.10.1 security review #10): the
/// player resamples to the speaker's rate, so a tiny claimed rate would
/// ask for an enormous buffer.
pub const MIN_RATE: u32 = 4_000;
pub const MAX_RATE: u32 = 384_000;
/// Channels a WAV file may have.
pub const MAX_CHANNELS: u16 = 32;
/// Samples Quire decodes or resamples to at most: 2²⁷ (512 MiB of
/// `f32`, about 46 minutes at 48 kHz). Longer audio is cut there.
pub const MAX_SAMPLES: usize = 1 << 27;

/// Decoded audio: mono samples and their rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Audio {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Audio {
    pub fn seconds(&self) -> f32 {
        if self.rate == 0 { 0.0 } else { self.samples.len() as f32 / self.rate as f32 }
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Read a WAV file's bytes.
pub fn decode(bytes: &[u8]) -> Result<Audio, String> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file".into());
    }
    let mut i = 12;
    let mut fmt = None;
    let mut data = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32_at(bytes, i + 4).ok_or("truncated WAV")? as usize;
        let body = i + 8;
        let end = body.saturating_add(len).min(bytes.len());
        match id {
            b"fmt " => {
                if len < 16 {
                    return Err("the WAV file's format chunk is too short".into());
                }
                let format = u16_at(bytes, body).ok_or("truncated fmt")?;
                let channels = u16_at(bytes, body + 2).ok_or("truncated fmt")?;
                let rate = u32_at(bytes, body + 4).ok_or("truncated fmt")?;
                let bits = u16_at(bytes, body + 14).ok_or("truncated fmt")?;
                // WAVE_FORMAT_EXTENSIBLE keeps the real format in its sub-format GUID.
                let format = if format == 0xFFFE { u16_at(bytes, body + 24).ok_or("truncated fmt")? } else { format };
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data = Some(&bytes[body..end]),
            _ => {}
        }
        i = body + len + (len & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or("the WAV file has no format chunk")?;
    let data = data.ok_or("the WAV file has no audio")?;
    if channels == 0 || rate == 0 {
        return Err("the WAV file has no channels".into());
    }
    if channels > MAX_CHANNELS {
        return Err(format!("the WAV file has {channels} channels; Quire plays up to {MAX_CHANNELS}"));
    }
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        return Err(format!("the WAV file's sample rate ({rate} Hz) is outside {MIN_RATE}–{MAX_RATE} Hz"));
    }
    let width = match bits {
        16 => 2,
        24 => 3,
        32 => 4,
        b => return Err(format!("unsupported WAV encoding (format {format}, {b} bits)")),
    };
    // Whole frames only, and no more than Quire decodes.
    let frames = (data.len() / width / usize::from(channels)).min(MAX_SAMPLES);
    let data = &data[..frames * width * usize::from(channels)];
    let frame: Vec<f32> = match (format, bits) {
        (1, 16) => data.chunks_exact(2).map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0).collect(),
        (1, 24) => data.chunks_exact(3).map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0).collect(),
        (1, 32) => {
            data.chunks_exact(4).map(|c| (f64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])) / 2_147_483_648.0) as f32).collect()
        }
        (3, 32) => data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        (f, b) => return Err(format!("unsupported WAV encoding (format {f}, {b} bits)")),
    };
    let ch = usize::from(channels);
    let samples = frame.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    Ok(Audio { samples, rate })
}

/// `samples` at `from` Hz resampled to `to` Hz (linear interpolation after a
/// box low-pass when shrinking; enough for speech recognition).
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    resample_at_most(samples, from, to, MAX_SAMPLES)
}

/// [`resample`] giving at most `cap` samples.
fn resample_at_most(samples: &[f32], from: u32, to: u32, cap: usize) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let smoothed: Vec<f32> = if ratio > 1.0 {
        let w = ratio.round().max(1.0) as usize;
        let mut acc = 0.0f32;
        let mut out = Vec::with_capacity(samples.len());
        for (i, s) in samples.iter().enumerate() {
            acc += s;
            if i >= w {
                acc -= samples[i - w];
            }
            out.push(acc / (i + 1).min(w) as f32);
        }
        out
    } else {
        samples.to_vec()
    };
    // Never more than [`MAX_SAMPLES`], whatever the rates say.
    let n = (((samples.len() as f64) / ratio).floor() as usize).min(cap);
    (0..n)
        .map(|i| {
            let pos = i as f64 * ratio;
            let j = pos.floor() as usize;
            let t = (pos - j as f64) as f32;
            let a = smoothed[j.min(smoothed.len() - 1)];
            let b = smoothed[(j + 1).min(smoothed.len() - 1)];
            a + (b - a) * t
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_pcm16() {
        let s: Vec<f32> = (0..1600).map(|i| (i as f32 / 10.0).sin() * 0.5).collect();
        let bytes = encode_pcm16(&s, VOICE_RATE);
        assert_eq!(bytes.len(), 44 + 3200);
        let a = decode(&bytes).unwrap();
        assert_eq!(a.rate, VOICE_RATE);
        assert_eq!(a.samples.len(), 1600);
        assert!(a.samples.iter().zip(&s).all(|(x, y)| (x - y).abs() < 1e-3));
        assert!((a.seconds() - 0.1).abs() < 1e-6);
    }

    #[test]
    fn reads_stereo_float_and_extensible_and_refuses_others() {
        // 2 channels, 32-bit float, WAVE_FORMAT_EXTENSIBLE.
        let frames = [[0.5f32, -0.5], [1.0, 0.0]];
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&0xFFFEu16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(48_000u32 * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes()); // sub-format: float
        b.extend_from_slice(&[0; 14]);
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[1, 2, 3, 0]); // odd chunk, padded
        b.extend_from_slice(b"data");
        b.extend_from_slice(&16u32.to_le_bytes());
        for f in frames {
            for s in f {
                b.extend_from_slice(&s.to_le_bytes());
            }
        }
        let a = decode(&b).unwrap();
        assert_eq!((a.rate, a.samples), (48_000, vec![0.0, 0.5]));
        assert!(decode(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(decode(b"OggS").is_err());
    }

    /// A WAV header in front of `data`: format, channels, rate, bits.
    fn wav(format: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&format.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&rate.wrapping_mul(u32::from(channels) * u32::from(bits / 8)).to_le_bytes());
        b.extend_from_slice(&(channels * (bits / 8)).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    /// 0.10.1 security review #10: a header can't make the player ask for
    /// a huge buffer.
    #[test]
    fn untrusted_headers_are_checked_and_resampling_is_capped() {
        let data = [0u8; 400];
        for rate in [1, 100, 3_999, 384_001, u32::MAX] {
            assert!(decode(&wav(1, 1, rate, 16, &data)).unwrap_err().contains("sample rate"), "{rate}");
        }
        assert!(decode(&wav(1, 1, 4_000, 16, &data)).is_ok());
        assert!(decode(&wav(1, 1, 384_000, 16, &data)).is_ok());
        assert!(decode(&wav(1, 33, 16_000, 16, &data)).unwrap_err().contains("channels"));
        assert!(decode(&wav(1, 1, 16_000, 8, &data)).is_err());
        assert!(decode(&wav(1, 1, 16_000, 0, &data)).is_err());
        // A short format chunk.
        let mut short = wav(1, 1, 16_000, 16, &data);
        short[16..20].copy_from_slice(&8u32.to_le_bytes());
        assert!(decode(&short).is_err());
        // A data chunk claiming more than the file has, and odd trailing bytes.
        let mut long = wav(1, 2, 16_000, 16, &[0u8; 7]);
        let at = long.len() - 7 - 4;
        long[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&long).unwrap().samples.len(), 1);
        // Resampling is bounded however far apart the rates are.
        assert_eq!(resample_at_most(&[0.0; 10], 1, 48_000, 1_000_000).len(), 480_000, "within the cap it is exact");
        assert_eq!(resample_at_most(&vec![0.0; 10_000], 1, 48_000, 1_000_000).len(), 1_000_000);
    }

    #[test]
    fn resamples_to_sixteen_kilohertz() {
        let s: Vec<f32> = vec![0.25; 48_000];
        let r = resample(&s, 48_000, VOICE_RATE);
        assert_eq!(r.len(), 16_000);
        assert!(r[100..].iter().all(|v| (v - 0.25).abs() < 1e-5));
        assert_eq!(resample(&[0.0, 1.0], 8_000, 16_000).len(), 4);
        assert_eq!(resample(&s, 16_000, 16_000).len(), 48_000);
    }
}
