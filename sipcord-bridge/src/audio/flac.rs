//! FLAC file parsing
//!
//! Parses FLAC file bytes to extract raw PCM i16 samples.

use anyhow::{bail, Context};
use tracing::debug;

/// Parse a FLAC file and return the raw PCM i16 samples (mono).
///
/// Handles:
/// - Standard FLAC files
/// - Stereo to mono conversion (if needed)
/// - Various bit depths (converted to 16-bit)
pub fn parse_flac(data: &[u8]) -> anyhow::Result<(Vec<i16>, u32)> {
    let cursor = std::io::Cursor::new(data);
    let mut reader = claxon::FlacReader::new(cursor).context("Failed to create FLAC reader")?;

    let info = reader.streaminfo();
    let sample_rate = info.sample_rate;
    let num_channels = info.channels as usize;
    let bits_per_sample = info.bits_per_sample;

    debug!(
        "FLAC format: {}Hz, {} channels, {} bits per sample",
        sample_rate, num_channels, bits_per_sample
    );

    // Read all samples
    let mut raw_samples: Vec<i32> = Vec::new();
    for sample in reader.samples() {
        raw_samples.push(sample.context("Failed to read FLAC sample")?);
    }

    // Convert to i16 based on bit depth
    let samples_i16: Vec<i16> = match bits_per_sample {
        8 => raw_samples.iter().map(|&s| (s << 8) as i16).collect(),
        16 => raw_samples.iter().map(|&s| s as i16).collect(),
        24 => raw_samples.iter().map(|&s| (s >> 8) as i16).collect(),
        32 => raw_samples.iter().map(|&s| (s >> 16) as i16).collect(),
        _ => bail!("Unsupported FLAC bit depth: {}", bits_per_sample),
    };

    // Convert to mono if stereo (samples are interleaved)
    let mono_samples = if num_channels == 2 {
        samples_i16
            .chunks(2)
            .map(|chunk| {
                if chunk.len() == 2 {
                    ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16
                } else {
                    chunk[0]
                }
            })
            .collect()
    } else if num_channels > 2 {
        // For more than 2 channels, take first channel only
        samples_i16
            .chunks(num_channels)
            .map(|chunk| chunk[0])
            .collect()
    } else {
        samples_i16
    };

    debug!(
        "FLAC data: {} samples ({}Hz, {} channels -> mono)",
        mono_samples.len(),
        sample_rate,
        num_channels
    );

    Ok((mono_samples, sample_rate))
}
