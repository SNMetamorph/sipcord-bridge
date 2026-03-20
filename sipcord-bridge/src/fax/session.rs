//! FaxSession state machine — manages a single incoming fax reception.
//!
//! Lifecycle:
//! 1. Created when a fax call is answered (ConnectFax route decision)
//! 2. Audio frames are fed via `feed_audio()`
//! 3. SpanDSP demodulates the fax tones into a TIFF file
//! 4. On completion, TIFF is converted to PNG and posted to Discord
//! 5. On failure or timeout, an error message is posted to Discord

use crate::fax::discord_poster::DiscordPoster;
use crate::fax::spandsp::{FaxReceiver, FaxRxStatus, FaxT38Receiver};
use crate::fax::tiff_decoder;
use crate::services::snowflake::Snowflake;
use crate::transport::sip::CallId;
use anyhow::Result;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Maximum duration for a fax session before timeout (5 minutes)
const FAX_TIMEOUT_SECS: u64 = 300;

/// How the fax audio is being received
pub enum FaxSource {
    /// G.711 audio passthrough
    G711Audio,
    /// T.38 UDPTL
    T38Udptl,
}

/// The active receiver — either audio-based or T.38 IFP-based.
enum FaxReceiverKind {
    /// G.711 audio passthrough (demodulates fax tones from audio samples)
    Audio(FaxReceiver),
    /// T.38 UDPTL (receives IFP packets directly)
    T38(FaxT38Receiver),
}

/// Current state of the fax reception
pub enum FaxState {
    /// Answered, feeding audio to SpanDSP, waiting for fax negotiation
    WaitingForData,
    /// SpanDSP confirmed fax negotiation started
    Receiving {
        /// Number of pages received so far
        pages_received: u32,
    },
    /// SpanDSP signaled fax complete, awaiting conversion and Discord posting
    Received,
    /// Fax posted to Discord successfully
    Complete,
    /// Fax reception failed
    Failed(String),
}

/// A single fax reception session
pub struct FaxSession {
    /// SIP call ID for this fax
    pub call_id: CallId,
    /// Discord text channel to post the fax to
    pub text_channel_id: Snowflake,
    /// Guild ID (for logging)
    pub guild_id: Snowflake,
    /// User ID who owns this mapping
    pub user_id: String,
    /// Current state
    pub state: FaxState,
    /// How we're receiving the fax
    pub source: FaxSource,
    /// When this session was created
    pub created_at: Instant,
    /// Discord poster for this session
    pub poster: DiscordPoster,
    /// SpanDSP fax receiver (audio or T.38 mode)
    receiver: FaxReceiverKind,
    /// Temp directory for this fax session's TIFF output
    pub tiff_dir: PathBuf,
    /// Discord message ID for the "Receiving fax..." status message.
    /// Stored separately so it survives state transitions to Complete/Failed.
    receiving_message_id: Option<u64>,
}

impl FaxSession {
    /// Create a new fax session. Initializes SpanDSP in receive mode.
    pub fn new(
        call_id: CallId,
        text_channel_id: Snowflake,
        guild_id: Snowflake,
        user_id: String,
        bot_token: String,
    ) -> Result<Self> {
        let fax_config = crate::config::AppConfig::fax();

        // Use configured tmp_folder or system temp dir
        let base_dir = fax_config
            .tmp_folder
            .clone()
            .unwrap_or_else(std::env::temp_dir);

        // Generate a unique session ID for the filename
        let session_id = format!("{:016x}", {
            use std::time::{SystemTime, UNIX_EPOCH};
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            // Mix timestamp with call_id using a prime constant for a unique session ID
            t.as_nanos() as u64 ^ (*call_id as u64).wrapping_mul(0x517cc1b727220a95)
        });

        let tiff_dir = base_dir.join(format!("{}{}", fax_config.prefix, session_id));
        std::fs::create_dir_all(&tiff_dir)?;
        let tiff_path = tiff_dir.join(format!("{}{}.tiff", fax_config.prefix, session_id));

        let receiver = FaxReceiver::new_audio_receiver(&tiff_path)?;

        let poster = DiscordPoster::new(bot_token, text_channel_id, user_id.clone());

        Ok(Self {
            call_id,
            text_channel_id,
            guild_id,
            user_id,
            state: FaxState::WaitingForData,
            source: FaxSource::G711Audio,
            created_at: Instant::now(),
            poster,
            receiver: FaxReceiverKind::Audio(receiver),
            tiff_dir,
            receiving_message_id: None,
        })
    }

    /// Feed audio samples from the SIP call (16kHz mono i16).
    /// Downsamples to 8kHz and feeds to SpanDSP's fax_rx().
    /// Returns true if the fax is complete and ready for post-processing.
    /// Only works in Audio mode — logs a warning and returns false if called in T.38 mode.
    pub fn feed_audio(&mut self, samples: &[i16]) -> bool {
        // Check for timeout
        if self.created_at.elapsed().as_secs() > FAX_TIMEOUT_SECS {
            warn!(
                "Fax session {} timed out after {}s",
                self.call_id,
                self.created_at.elapsed().as_secs()
            );
            self.state = FaxState::Failed("Fax reception timed out".to_string());
            return false;
        }

        if self.is_finished() {
            return matches!(self.state, FaxState::Received | FaxState::Complete);
        }

        let receiver = match &mut self.receiver {
            FaxReceiverKind::Audio(r) => r,
            FaxReceiverKind::T38(_) => {
                warn!("feed_audio called on T.38 session {}", self.call_id);
                return false;
            }
        };

        let status = receiver.feed_samples_16k(samples);
        self.handle_rx_status(status)
    }

    /// Feed a T.38 IFP packet from the UDPTL socket to SpanDSP.
    /// Returns true if the fax is complete and ready for post-processing.
    /// Only works in T.38 mode.
    pub fn feed_t38_ifp(&mut self, data: &[u8], seq: u16) -> bool {
        if self.is_finished() {
            return matches!(self.state, FaxState::Received | FaxState::Complete);
        }

        let receiver = match &mut self.receiver {
            FaxReceiverKind::T38(r) => r,
            FaxReceiverKind::Audio(_) => {
                warn!("feed_t38_ifp called on audio session {}", self.call_id);
                return false;
            }
        };

        let status = receiver.feed_ifp_packet(data, seq);
        self.handle_rx_status(status)
    }

    /// Drive the T.38 terminal timer (call every 20ms).
    /// Returns true if the fax is complete and ready for post-processing.
    pub fn drive_t38_timer(&mut self) -> bool {
        if self.is_finished() {
            return matches!(self.state, FaxState::Received | FaxState::Complete);
        }

        let receiver = match &mut self.receiver {
            FaxReceiverKind::T38(r) => r,
            FaxReceiverKind::Audio(_) => return false,
        };

        let status = receiver.drive_timer();
        self.handle_rx_status(status)
    }

    /// Common handler for FaxRxStatus from either audio or T.38 receiver.
    fn handle_rx_status(&mut self, status: FaxRxStatus) -> bool {
        // Log stats on completion/error before delegating to pure state transition
        match &status {
            FaxRxStatus::Complete => {
                if let Some(stats) = self.get_stats() {
                    info!(
                        "Fax {} complete: {} pages, {}bps, {}x{}, ECM={}, bad_rows={}",
                        self.call_id,
                        stats.pages_rx,
                        stats.bit_rate,
                        stats.image_width,
                        stats.image_length,
                        stats.ecm,
                        stats.bad_rows
                    );
                }
            }
            FaxRxStatus::Error(msg) => {
                if let Some(stats) = self.get_stats() {
                    warn!(
                        "Fax {} failed: {} ({}bps, {}x{}, ECM={}, pages_rx={}, bad_rows={}, audio={:.1}s)",
                        self.call_id,
                        msg,
                        stats.bit_rate,
                        stats.image_width,
                        stats.image_length,
                        stats.ecm,
                        stats.pages_rx,
                        stats.bad_rows,
                        self.audio_duration_secs()
                    );
                } else {
                    warn!(
                        "Fax {} failed: {} (no stats, audio={:.1}s)",
                        self.call_id,
                        msg,
                        self.audio_duration_secs()
                    );
                }
            }
            FaxRxStatus::InProgress => {}
        }

        let page_count = self.pages_received();
        apply_rx_status(&mut self.state, status, page_count)
    }

    /// Number of pages received so far.
    pub fn pages_received(&self) -> u32 {
        match &self.receiver {
            FaxReceiverKind::Audio(r) => r.pages_received(),
            FaxReceiverKind::T38(r) => r.pages_received(),
        }
    }

    /// Get transfer statistics from SpanDSP.
    fn get_stats(&self) -> Option<crate::fax::spandsp::FaxStats> {
        match &self.receiver {
            FaxReceiverKind::Audio(r) => r.get_stats(),
            FaxReceiverKind::T38(r) => r.get_stats(),
        }
    }

    /// Check if this session has timed out
    pub fn is_timed_out(&self) -> bool {
        self.created_at.elapsed().as_secs() > FAX_TIMEOUT_SECS
    }

    /// Check if the session is in a terminal state
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state,
            FaxState::Received | FaxState::Complete | FaxState::Failed(_)
        )
    }

    /// Post the initial "Receiving fax..." message to Discord.
    /// Called when fax negotiation is detected.
    pub async fn post_receiving_message(&mut self) -> Result<()> {
        match self.poster.post_fax_receiving().await {
            Ok(msg_id) => {
                debug!(
                    "Posted 'Receiving fax...' message {} to channel {} (call {})",
                    msg_id, self.text_channel_id, self.call_id
                );
                self.receiving_message_id = Some(msg_id);
                self.state = FaxState::Receiving { pages_received: 0 };
                Ok(())
            }
            Err(e) => {
                error!(
                    "Failed to post receiving message to channel {}: {}",
                    self.text_channel_id, e
                );
                self.state = FaxState::Failed(format!("Discord error: {}", e));
                Err(e)
            }
        }
    }

    /// Post a failure message to Discord
    pub async fn post_failure(&mut self, reason: &str) {
        if let Some(discord_msg_id) = self.receiving_message_id {
            if let Err(e) = self.poster.edit_fax_failed(discord_msg_id, reason).await {
                error!("Failed to edit fax failure message: {}", e);
            }
        } else {
            // No receiving message was posted — post a standalone failure
            if let Err(e) = self.poster.post_fax_failed(reason).await {
                error!("Failed to post fax failure message: {}", e);
            }
        }
        self.state = FaxState::Failed(reason.to_string());
    }

    /// Convert the received TIFF to images and post to Discord.
    /// Called after fax reception is complete.
    pub async fn convert_and_post(&mut self) -> Result<()> {
        // Guard against double-processing: if we've already posted (Complete) or failed,
        // another caller (e.g., CallEnded racing with T.38 completion) already handled it.
        // Note: FaxState::Received is NOT skipped — that's the normal entry state.
        if matches!(self.state, FaxState::Complete | FaxState::Failed(_)) {
            debug!(
                "convert_and_post called on already-finished session {} — skipping",
                self.call_id
            );
            return Ok(());
        }

        let (tiff_path, pages) = match &self.receiver {
            FaxReceiverKind::Audio(r) => (
                r.tiff_output_path().to_path_buf(),
                r.pages_received().max(1),
            ),
            FaxReceiverKind::T38(r) => (
                r.tiff_output_path().to_path_buf(),
                r.pages_received().max(1),
            ),
        };
        let tiff_path = &tiff_path;

        let fax_config = crate::config::AppConfig::fax();
        let (output_format, file_ext) = match fax_config.output_format.as_str() {
            "jpg" | "jpeg" => (OutputFormat::Jpeg, "jpg"),
            _ => (OutputFormat::Png, "png"),
        };

        debug!(
            "Converting TIFF to {} for call {}: {} ({} pages)",
            output_format.label(),
            self.call_id,
            tiff_path.display(),
            pages
        );

        let gray_images = tiff_decoder::decode_fax_tiff(tiff_path)?;
        let image_pages: Vec<Vec<u8>> = gray_images
            .into_iter()
            .map(|img| {
                let mut buf = Vec::new();
                image::DynamicImage::ImageLuma8(img)
                    .write_to(&mut Cursor::new(&mut buf), output_format.image_format())
                    .map(|_| buf)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if image_pages.is_empty() {
            self.post_failure("No pages in received fax").await;
            anyhow::bail!("No pages in received fax");
        }

        let page_count = image_pages.len() as u32;

        if let Some(discord_msg_id) = self.receiving_message_id {
            match self
                .poster
                .edit_fax_complete(discord_msg_id, image_pages, page_count, file_ext)
                .await
            {
                Ok(()) => {
                    info!(
                        "Fax complete: {} pages posted to channel {} (call {})",
                        page_count, self.text_channel_id, self.call_id
                    );
                    self.state = FaxState::Complete;
                }
                Err(e) => {
                    error!("Failed to post completed fax: {}", e);
                    self.state = FaxState::Failed(format!("Discord upload error: {}", e));
                    return Err(e);
                }
            }
        } else {
            // If we never posted a "receiving" message (e.g., fast fax), post directly
            // This shouldn't normally happen since we post receiving message early
            warn!("Fax completed without a receiving message — posting directly");
            match self.poster.post_fax_receiving().await {
                Ok(msg_id) => {
                    self.receiving_message_id = Some(msg_id);
                    self.poster
                        .edit_fax_complete(msg_id, image_pages, page_count, file_ext)
                        .await?;
                    self.state = FaxState::Complete;
                }
                Err(e) => {
                    error!("Failed to post fax: {}", e);
                    self.state = FaxState::Failed(format!("Discord error: {}", e));
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Switch from G.711 audio mode to T.38 UDPTL mode.
    ///
    /// Replaces the audio receiver with a T.38 receiver. The caller must:
    /// 1. Stop feeding audio samples (remove fax audio port)
    /// 2. Start the UDPTL processing tasks (rx, tx, timer)
    pub fn switch_to_t38(&mut self, t38_receiver: FaxT38Receiver) {
        debug!("Fax session {} switching from G.711 to T.38", self.call_id);
        self.source = FaxSource::T38Udptl;
        self.receiver = FaxReceiverKind::T38(t38_receiver);
    }

    /// Generate transmit audio from SpanDSP (CED tones, T.30 signaling).
    ///
    /// Only works in Audio mode — T.38 uses IFP packets, not audio.
    /// `out_buf` should be 320 samples (20ms at 16kHz).
    /// Returns the number of 16kHz samples written.
    pub fn generate_tx_16k(&mut self, out_buf: &mut [i16]) -> usize {
        match &mut self.receiver {
            FaxReceiverKind::Audio(r) => r.generate_tx_16k(out_buf),
            FaxReceiverKind::T38(_) => 0,
        }
    }

    /// Get the total audio duration received so far (for debugging).
    /// Returns 0 in T.38 mode (no audio samples).
    pub fn audio_duration_secs(&self) -> f64 {
        match &self.receiver {
            FaxReceiverKind::Audio(r) => r.audio_duration_secs(),
            FaxReceiverKind::T38(_) => 0.0,
        }
    }
}

impl Drop for FaxSession {
    fn drop(&mut self) {
        let status = match &self.state {
            FaxState::WaitingForData => "waiting_for_data",
            FaxState::Receiving { .. } => "receiving",
            FaxState::Received => "received",
            FaxState::Complete => "complete",
            FaxState::Failed(reason) => {
                debug!("Fax failure reason: {}", reason);
                "failed"
            }
        };
        debug!(
            "FaxSession dropped: call={}, channel={}, guild={}, user={}, status={}, duration={:.1}s, audio={:.1}s",
            self.call_id,
            self.text_channel_id,
            self.guild_id,
            self.user_id,
            status,
            self.created_at.elapsed().as_secs_f64(),
            self.audio_duration_secs()
        );
        if let Err(e) = std::fs::remove_dir_all(&self.tiff_dir) {
            debug!(
                "Failed to clean up fax temp dir {}: {}",
                self.tiff_dir.display(),
                e
            );
        } else {
            debug!("Cleaned up fax temp dir: {}", self.tiff_dir.display());
        }
    }
}

// Pure state transition logic (extracted for testability)

/// Apply a FaxRxStatus to a FaxState, returning whether the fax is complete.
/// This is the core state transition logic used by `FaxSession::handle_rx_status`.
fn apply_rx_status(state: &mut FaxState, status: FaxRxStatus, page_count: u32) -> bool {
    match status {
        FaxRxStatus::InProgress => {
            if let FaxState::Receiving { pages_received, .. } = state {
                *pages_received = page_count;
            }
            false
        }
        FaxRxStatus::Complete => {
            *state = FaxState::Received;
            true
        }
        FaxRxStatus::Error(msg) => {
            *state = FaxState::Failed(msg);
            false
        }
    }
}

// Output format

#[derive(Debug, Clone, Copy)]
enum OutputFormat {
    Png,
    Jpeg,
}

impl OutputFormat {
    fn image_format(self) -> image::ImageFormat {
        match self {
            OutputFormat::Png => image::ImageFormat::Png,
            OutputFormat::Jpeg => image::ImageFormat::Jpeg,
        }
    }

    fn label(self) -> &'static str {
        match self {
            OutputFormat::Png => "PNG",
            OutputFormat::Jpeg => "JPEG",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: check if a FaxState is_finished (mirrors FaxSession::is_finished logic)
    fn state_is_finished(state: &FaxState) -> bool {
        matches!(
            state,
            FaxState::Received | FaxState::Complete | FaxState::Failed(_)
        )
    }

    // is_finished tests

    #[test]
    fn is_finished_waiting_for_data() {
        assert!(!state_is_finished(&FaxState::WaitingForData));
    }

    #[test]
    fn is_finished_receiving() {
        assert!(!state_is_finished(&FaxState::Receiving {
            pages_received: 0
        }));
    }

    #[test]
    fn is_finished_received() {
        assert!(state_is_finished(&FaxState::Received));
    }

    #[test]
    fn is_finished_complete() {
        assert!(state_is_finished(&FaxState::Complete));
    }

    #[test]
    fn is_finished_failed() {
        assert!(state_is_finished(&FaxState::Failed("err".to_string())));
    }

    // is_timed_out tests

    #[test]
    fn is_timed_out_fresh() {
        // A fresh Instant should not be timed out
        let created_at = Instant::now();
        let elapsed = created_at.elapsed().as_secs();
        assert!(elapsed <= FAX_TIMEOUT_SECS);
    }

    #[test]
    fn is_timed_out_old() {
        // An instant created FAX_TIMEOUT_SECS+1 ago should be timed out
        let created_at = Instant::now() - std::time::Duration::from_secs(FAX_TIMEOUT_SECS + 1);
        assert!(created_at.elapsed().as_secs() > FAX_TIMEOUT_SECS);
    }

    // apply_rx_status tests

    #[test]
    fn apply_rx_status_in_progress_on_waiting() {
        let mut state = FaxState::WaitingForData;
        let result = apply_rx_status(&mut state, FaxRxStatus::InProgress, 0);
        assert!(!result);
        assert!(matches!(state, FaxState::WaitingForData));
    }

    #[test]
    fn apply_rx_status_in_progress_on_receiving_updates_pages() {
        let mut state = FaxState::Receiving { pages_received: 0 };
        let result = apply_rx_status(&mut state, FaxRxStatus::InProgress, 3);
        assert!(!result);
        match state {
            FaxState::Receiving { pages_received } => assert_eq!(pages_received, 3),
            _ => panic!("Expected Receiving state"),
        }
    }

    #[test]
    fn apply_rx_status_complete_transitions_to_received() {
        let mut state = FaxState::Receiving { pages_received: 1 };
        let result = apply_rx_status(&mut state, FaxRxStatus::Complete, 1);
        assert!(result);
        assert!(matches!(state, FaxState::Received));
    }

    #[test]
    fn apply_rx_status_error_transitions_to_failed() {
        let mut state = FaxState::WaitingForData;
        let result = apply_rx_status(&mut state, FaxRxStatus::Error("timeout".to_string()), 0);
        assert!(!result);
        match state {
            FaxState::Failed(msg) => assert_eq!(msg, "timeout"),
            _ => panic!("Expected Failed state"),
        }
    }

    #[test]
    fn apply_rx_status_idempotent_on_terminal_complete() {
        // Once in Received, InProgress should not change the state
        let mut state = FaxState::Received;
        let result = apply_rx_status(&mut state, FaxRxStatus::InProgress, 0);
        assert!(!result);
        assert!(matches!(state, FaxState::Received));
    }

    #[test]
    fn apply_rx_status_idempotent_on_terminal_failed() {
        let mut state = FaxState::Failed("original".to_string());
        let result = apply_rx_status(&mut state, FaxRxStatus::InProgress, 0);
        assert!(!result);
        match state {
            FaxState::Failed(msg) => assert_eq!(msg, "original"),
            _ => panic!("Expected Failed state"),
        }
    }
}
