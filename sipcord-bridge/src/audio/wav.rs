//! WAV file parsing
//!
//! Parses WAV file bytes to extract raw PCM i16 samples.
//! Supports standard PCM WAV files (format code 1).

use anyhow::ensure;
use tracing::debug;

/// WAV format chunk data
#[derive(Debug)]
struct WavFormat {
    /// Audio format (1 = PCM)
    audio_format: u16,
    /// Number of channels
    num_channels: u16,
    /// Sample rate in Hz
    sample_rate: u32,
    /// Bits per sample (typically 16)
    bits_per_sample: u16,
}

/// Parse a WAV file and return the raw PCM i16 samples (mono).
///
/// Handles:
/// - Standard PCM WAV files (format code 1)
/// - Stereo to mono conversion (if needed)
/// - 16-bit samples
pub fn parse_wav(data: &[u8]) -> anyhow::Result<(Vec<i16>, u32)> {
    // Validate RIFF header
    ensure!(data.len() >= 12, "WAV file too short for header");
    ensure!(&data[0..4] == b"RIFF", "Missing RIFF header");
    ensure!(&data[8..12] == b"WAVE", "Missing WAVE format");

    let mut pos = 12;
    let mut format: Option<WavFormat> = None;
    let mut samples: Vec<i16> = Vec::new();

    // Parse chunks
    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        pos += 8;

        match chunk_id {
            b"fmt " => {
                ensure!(chunk_size >= 16, "fmt chunk too small");
                format = Some(WavFormat {
                    audio_format: u16::from_le_bytes([data[pos], data[pos + 1]]),
                    num_channels: u16::from_le_bytes([data[pos + 2], data[pos + 3]]),
                    sample_rate: u32::from_le_bytes([
                        data[pos + 4],
                        data[pos + 5],
                        data[pos + 6],
                        data[pos + 7],
                    ]),
                    // Skip byte rate (4 bytes) and block align (2 bytes)
                    bits_per_sample: u16::from_le_bytes([data[pos + 14], data[pos + 15]]),
                });
                debug!("WAV format: {:?}", format);
            }
            b"data" => {
                let fmt = format.as_ref().ok_or_else(|| anyhow::anyhow!("data chunk before fmt chunk"))?;
                ensure!(fmt.audio_format == 1, "Only PCM format supported");
                ensure!(fmt.bits_per_sample == 16, "Only 16-bit samples supported");

                let data_end = (pos + chunk_size).min(data.len());
                let sample_data = &data[pos..data_end];

                // Parse i16 samples
                let raw_samples: Vec<i16> = sample_data
                    .chunks_exact(2)
                    .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();

                // Convert to mono if stereo
                samples = if fmt.num_channels == 2 {
                    raw_samples
                        .chunks(2)
                        .map(|chunk| {
                            if chunk.len() == 2 {
                                ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16
                            } else {
                                chunk[0]
                            }
                        })
                        .collect()
                } else {
                    raw_samples
                };

                debug!(
                    "WAV data: {} samples ({}Hz, {} channels -> mono)",
                    samples.len(),
                    fmt.sample_rate,
                    fmt.num_channels
                );
            }
            _ => {
                // Skip unknown chunks
                debug!("Skipping WAV chunk: {:?}", std::str::from_utf8(chunk_id));
            }
        }

        // Move to next chunk (chunks are word-aligned)
        pos += chunk_size;
        if !chunk_size.is_multiple_of(2) {
            pos += 1;
        }
    }

    let sample_rate = format
        .as_ref()
        .map(|f| f.sample_rate)
        .ok_or_else(|| anyhow::anyhow!("No fmt chunk found"))?;

    Ok((samples, sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_wav() {
        // Minimal valid WAV with 4 samples of silence
        let wav = [
            // RIFF header
            b'R', b'I', b'F', b'F', // "RIFF"
            0x2C, 0x00, 0x00, 0x00, // File size - 8 = 44
            b'W', b'A', b'V', b'E', // "WAVE"
            // fmt chunk
            b'f', b'm', b't', b' ', // "fmt "
            0x10, 0x00, 0x00, 0x00, // Chunk size = 16
            0x01, 0x00, // Audio format = 1 (PCM)
            0x01, 0x00, // Num channels = 1 (mono)
            0x80, 0x3E, 0x00, 0x00, // Sample rate = 16000
            0x00, 0x7D, 0x00, 0x00, // Byte rate = 32000
            0x02, 0x00, // Block align = 2
            0x10, 0x00, // Bits per sample = 16
            // data chunk
            b'd', b'a', b't', b'a', // "data"
            0x08, 0x00, 0x00, 0x00, // Chunk size = 8 bytes = 4 samples
            0x00, 0x00, // Sample 0 = 0
            0x00, 0x10, // Sample 1 = 4096
            0x00, 0x20, // Sample 2 = 8192
            0x00, 0x30, // Sample 3 = 12288
        ];

        let (samples, rate) = parse_wav(&wav).unwrap();
        assert_eq!(rate, 16000);
        assert_eq!(samples.len(), 4);
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1], 4096);
        assert_eq!(samples[2], 8192);
        assert_eq!(samples[3], 12288);
    }

    #[test]
    fn test_parse_wav_too_short() {
        let result = parse_wav(&[0u8; 4]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_parse_wav_wrong_magic() {
        let mut data = [0u8; 44];
        data[0..4].copy_from_slice(b"NOPE");
        let result = parse_wav(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("RIFF"));
    }

    #[test]
    fn test_parse_stereo_wav() {
        // Stereo WAV: 2 stereo sample frames = 4 raw samples -> 2 mono samples
        let wav = [
            // RIFF header
            b'R', b'I', b'F', b'F', 0x2C, 0x00, 0x00, 0x00, // File size - 8
            b'W', b'A', b'V', b'E', // fmt chunk
            b'f', b'm', b't', b' ', 0x10, 0x00, 0x00, 0x00, // Chunk size = 16
            0x01, 0x00, // PCM
            0x02, 0x00, // 2 channels (stereo)
            0x80, 0x3E, 0x00, 0x00, // 16000 Hz
            0x00, 0xFA, 0x00, 0x00, // Byte rate = 64000
            0x04, 0x00, // Block align = 4
            0x10, 0x00, // 16 bits
            // data chunk
            b'd', b'a', b't', b'a', 0x08, 0x00, 0x00, 0x00, // 8 bytes = 2 stereo frames
            // Frame 1: L=1000, R=3000 -> mono = 2000
            0xE8, 0x03, // 1000 LE
            0xB8, 0x0B, // 3000 LE
            // Frame 2: L=-100, R=100 -> mono = 0
            0x9C, 0xFF, // -100 LE
            0x64, 0x00, // 100 LE
        ];

        let (samples, rate) = parse_wav(&wav).unwrap();
        assert_eq!(rate, 16000);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], 2000);
        assert_eq!(samples[1], 0);
    }
}
