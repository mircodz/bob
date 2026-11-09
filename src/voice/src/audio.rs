//! Microphone capture via cpal.
//!
//! Opens the default input device and streams mono f32 samples over a channel.
//! Input is resampled naively to ~16 kHz (decimation) since STT models expect
//! 16 kHz mono. For production, swap the decimation for a real resampler
//! (e.g. `rubato`).

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use crossbeam_channel::{Receiver, Sender};

/// Target sample rate expected by the STT models.
pub const TARGET_SR: u32 = 16_000;

/// A running capture session. Dropping it stops the stream.
pub struct AudioCapture {
    _stream: Stream,
    pub samples: Receiver<Vec<f32>>,
}

impl AudioCapture {
    /// Start capturing from the default input device.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        let config = device.default_input_config()?;
        let src_sr = config.sample_rate().0;
        let channels = config.channels() as usize;
        let decim = (src_sr / TARGET_SR).max(1) as usize;

        let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(32);
        // Stream errors must not print: the host may own the terminal (e.g. a TUI).
        let err_fn = |_e| {};

        let stream = match config.sample_format() {
            SampleFormat::F32 => device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| downmix_decimate(data, channels, decim, &tx),
                err_fn,
                None,
            )?,
            SampleFormat::I16 => device.build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    downmix_decimate(&f, channels, decim, &tx);
                },
                err_fn,
                None,
            )?,
            other => return Err(anyhow!("unsupported sample format: {other:?}")),
        };

        stream.play()?;
        Ok(Self {
            _stream: stream,
            samples: rx,
        })
    }
}

/// Downmix interleaved channels to mono and decimate to ~16 kHz.
fn downmix_decimate(data: &[f32], channels: usize, decim: usize, tx: &Sender<Vec<f32>>) {
    let mut out = Vec::with_capacity(data.len() / (channels * decim) + 1);
    let mut i = 0;
    while i + channels <= data.len() {
        let mut acc = 0.0;
        for c in 0..channels {
            acc += data[i + c];
        }
        out.push(acc / channels as f32);
        i += channels * decim;
    }
    // Non-blocking: drop a chunk rather than stall the audio callback.
    let _ = tx.try_send(out);
}

/// Minimal WAV reader (16-bit PCM): downmix to mono, decimate to ~16 kHz.
/// Enough for the offline path and tests; not a general audio decoder.
pub fn read_wav_16k_mono(path: &std::path::Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE file: {}", path.display());
    }
    let mut pos = 12;
    let mut channels = 1u16;
    let mut sample_rate = 16_000u32;
    let mut bits = 16u16;
    let mut data: &[u8] = &[];
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());
        match id {
            b"fmt " if size >= 16 => {
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
            }
            b"data" => data = &bytes[body_start..body_end],
            _ => {}
        }
        pos = body_end + (size & 1); // chunks are word-aligned
    }
    if bits != 16 {
        anyhow::bail!("only 16-bit PCM WAV supported (got {bits}-bit)");
    }
    let ch = channels.max(1) as usize;
    let decim = (sample_rate / TARGET_SR).max(1) as usize;
    let mut out = Vec::new();
    let frame_bytes = 2 * ch;
    let mut i = 0;
    while i + frame_bytes <= data.len() {
        let mut acc = 0.0f32;
        for c in 0..ch {
            let s = i16::from_le_bytes([data[i + 2 * c], data[i + 2 * c + 1]]);
            acc += s as f32 / i16::MAX as f32;
        }
        out.push(acc / ch as f32);
        i += frame_bytes * decim;
    }
    Ok(out)
}
