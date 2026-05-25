//! Audio parsing utilities
//!
//! This module provides audio file parsing for WAV and FLAC formats.
//! Used by the `sound` module for loading audio files from disk.

pub mod flac;
pub mod simd;
pub mod wav;

/// Errors that can occur while parsing a WAV or FLAC file.
#[derive(thiserror::Error, Debug)]
pub enum AudioParseError {
    /// File header, chunk structure, or sample data was malformed for the
    /// format implied by the magic bytes. Carries a short human-readable
    /// reason (chunk name, byte offset, etc.).
    #[error("malformed audio data: {0}")]
    Malformed(String),

    /// Audio format is recognised but not supported by this parser (e.g.
    /// non-PCM WAV, or a FLAC stream with an exotic bit depth).
    #[error("unsupported audio: {0}")]
    Unsupported(String),

    /// Underlying claxon FLAC decoder error.
    #[error("FLAC decode error: {0}")]
    Flac(#[from] claxon::Error),
}
