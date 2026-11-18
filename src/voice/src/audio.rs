//! Microphone capture via cpal.
//!
//! Opens the default input device and streams mono f32 samples at 16 kHz, the
//! rate the STT models expect, resampling by linear interpolation.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream};
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
        let source_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(32);
        let stream = match config.sample_format() {
            SampleFormat::F32 => input::<f32>(&device, &config, channels, source_rate, tx),
            SampleFormat::I16 => input::<i16>(&device, &config, channels, source_rate, tx),
            SampleFormat::I32 => input::<i32>(&device, &config, channels, source_rate, tx),
            SampleFormat::U16 => input::<u16>(&device, &config, channels, source_rate, tx),
            SampleFormat::U8 => input::<u8>(&device, &config, channels, source_rate, tx),
            SampleFormat::F64 => input::<f64>(&device, &config, channels, source_rate, tx),
            other => return Err(anyhow!("unsupported sample format: {other:?}")),
        }?;

        stream.play()?;
        Ok(Self {
            _stream: stream,
            samples: rx,
        })
    }
}

fn input<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    channels: usize,
    source_rate: u32,
    tx: Sender<Vec<f32>>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut resampler = Resampler::new(source_rate);
    let mut mono = Vec::new();
    Ok(device.build_input_stream(
        &config.config(),
        move |data: &[T], _| {
            mono.clear();
            mono.extend(data.chunks_exact(channels).map(|frame| {
                frame
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .sum::<f32>()
                    / channels as f32
            }));
            // Non-blocking: drop a chunk rather than stall the audio callback.
            let _ = tx.try_send(resampler.push(&mono));
        },
        // Stream errors must not print: the host may own the terminal (e.g. a TUI).
        |_| {},
        None,
    )?)
}

/// Converts mono audio at any rate to 16 kHz by linear interpolation, one
/// chunk at a time, carrying its position across chunks.
struct Resampler {
    step: f64,
    position: f64,
    previous: Option<f32>,
}

impl Resampler {
    fn new(source_rate: u32) -> Self {
        Self {
            step: f64::from(source_rate.max(1)) / f64::from(TARGET_SR),
            position: 0.0,
            previous: None,
        }
    }

    /// `position` counts from the carried-over last sample of the previous
    /// chunk (index 0) when there is one, otherwise from this chunk's start.
    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        let mut frames = Vec::with_capacity(input.len() + 1);
        frames.extend(self.previous);
        frames.extend_from_slice(input);
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);
        while self.position + 1.0 < frames.len() as f64 {
            let index = self.position as usize;
            let fraction = (self.position - index as f64) as f32;
            out.push(frames[index] + (frames[index + 1] - frames[index]) * fraction);
            self.position += self.step;
        }
        if let Some(&last) = frames.last() {
            self.position -= (frames.len() - 1) as f64;
            self.previous = Some(last);
        }
        out
    }
}

/// Minimal WAV reader (16-bit PCM): downmix to mono, resample to 16 kHz.
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
    let mono: Vec<f32> = data
        .chunks_exact(2 * ch)
        .map(|frame| {
            frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| f32::from_sample(i16::from_le_bytes(*sample)))
                .sum::<f32>()
                / ch as f32
        })
        .collect();
    Ok(Resampler::new(sample_rate).push(&mono))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, seconds: f64) -> Vec<f32> {
        (0..(f64::from(rate) * seconds) as usize)
            .map(|n| (2.0 * std::f64::consts::PI * 440.0 * n as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    #[test]
    fn every_common_rate_becomes_16_khz_without_changing_speed() {
        for rate in [8_000, 16_000, 22_050, 24_000, 44_100, 48_000, 96_000] {
            let input = tone(rate, 2.0);
            let mut resampler = Resampler::new(rate);
            let output: Vec<f32> = input
                .chunks(rate as usize / 100 + 7)
                .flat_map(|chunk| resampler.push(chunk))
                .collect();
            let expected = 2 * TARGET_SR as usize;
            assert!(
                output.len().abs_diff(expected) <= 2,
                "{rate} Hz: {} samples, expected {expected}",
                output.len()
            );
            for (n, sample) in output.iter().enumerate() {
                let ideal =
                    (2.0 * std::f64::consts::PI * 440.0 * n as f64 / f64::from(TARGET_SR)).sin();
                assert!(
                    (f64::from(*sample) - ideal).abs() < 0.05,
                    "{rate} Hz: sample {n} is {sample}, expected {ideal}"
                );
            }
        }
    }
}
