//! Incoming fax support — receives faxes over SIP and posts images to Discord.
//!
//! Supports two transport modes:
//! - **G.711 passthrough**: Demodulates fax tones from audio samples (SpanDSP FaxState)
//! - **T.38 native**: Receives IFP packets via UDPTL (SpanDSP T38Terminal)
//!
//! Architecture:
//! - FaxSession: State machine managing a single fax reception (audio or T.38)
//! - DiscordPoster: Posts/edits messages in Discord text channels with fax images
//! - SpanDSP wrapper: FFI to SpanDSP for fax demodulation (FaxReceiver + FaxT38Receiver)
//! - audio_port: Conference bridge port for capturing SIP audio (G.711 mode)
//! - UDPTL: UDP transport for T.38 IFP packets

pub mod audio_port;
pub mod discord_poster;
pub mod session;
pub mod spandsp;
pub mod tiff_decoder;

/// Errors from the fax subsystem.
///
/// Variants are intentionally coarse — fax flows are end-to-end best-effort
/// (a missed page or codec mismatch logs and aborts the session) and the
/// detailed `String` payloads carry enough context for triage. Where a more
/// structured upstream type already exists (`serenity::Error`, `io::Error`),
/// we wrap it via `#[from]` / `#[source]`.
#[derive(thiserror::Error, Debug)]
pub enum FaxError {
    /// Discord REST / gateway error while posting or editing fax status.
    #[error("Discord post failed: {0}")]
    Discord(#[from] serenity::Error),

    /// Token parsing failure when constructing the fax-posting client.
    #[error("invalid Discord bot token: {0}")]
    InvalidToken(String),

    /// I/O error reading/writing TIFFs or working with paths.
    #[error("fax I/O ({context}): {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// Path couldn't be converted to UTF-8 for the SpanDSP / TIFF API.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),

    /// SpanDSP FFI returned an error from one of its setters or state-init
    /// functions.
    #[error("SpanDSP ({operation}): {detail}")]
    SpanDsp {
        operation: &'static str,
        detail: String,
    },

    /// TIFF parsing / decoding failure. Carries a human-readable reason.
    #[error("TIFF decode: {0}")]
    Tiff(String),

    /// A received fax produced no pages (decoder bail-out, session closed
    /// before any page was completed, etc.).
    #[error("no pages in received fax")]
    NoPages,
}
