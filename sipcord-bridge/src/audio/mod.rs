//! Audio parsing utilities
//!
//! This module provides audio file parsing for WAV and FLAC formats.
//! Used by the `sound` module for loading audio files from disk.

pub mod flac;
pub mod simd;
pub mod wav;
