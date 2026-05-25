//! Discord message poster for fax sessions using serenity's HTTP client.
//!
//! Posts embed messages through the fax lifecycle:
//! - "Receiving fax..." (blurple) when negotiation starts
//! - Replaced with "Fax Received" (green) with page image gallery on success
//! - Edited to "Fax Failed" (red) with reason on failure

use super::FaxError;
use crate::services::snowflake::Snowflake;
use serenity::all::{ChannelId, MessageId, UserId};
use serenity::builder::{
    CreateAttachment, CreateEmbed, CreateEmbedFooter, CreateMessage, EditMessage,
};
use serenity::http::Http;
use serenity::secrets::Token;
use std::sync::Arc;
use tracing::{debug, error, warn};

const COLOR_RECEIVING: u32 = 0x5865F2; // Discord blurple
const COLOR_COMPLETE: u32 = 0x57F287; // Green
const COLOR_FAILED: u32 = 0xED4245; // Red
const GALLERY_URL: &str = "https://sipcord.net/fax";

pub struct DiscordPoster {
    http: Arc<Http>,
    channel_id: ChannelId,
    user_id: String,
    /// Cached display name, resolved on first use
    display_name: Option<String>,
}

impl DiscordPoster {
    pub fn new(
        bot_token: String,
        channel_id: Snowflake,
        user_id: String,
    ) -> Result<Self, FaxError> {
        let token: Token = bot_token
            .parse()
            .map_err(|e| FaxError::InvalidToken(format!("{e}")))?;
        Ok(Self {
            http: Arc::new(Http::new(token)),
            channel_id: ChannelId::new(*channel_id),
            user_id,
            display_name: None,
        })
    }

    /// Resolve and cache the Discord display name for the user.
    async fn resolve_display_name(&mut self) {
        if self.display_name.is_some() {
            return;
        }
        let name = match self.user_id.parse::<u64>() {
            Ok(id) => match UserId::new(id).to_user(&self.http).await {
                Ok(user) => user
                    .global_name
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| user.name.to_string()),
                Err(e) => {
                    warn!("Failed to resolve Discord user {}: {}", self.user_id, e);
                    self.user_id.clone()
                }
            },
            Err(_) => self.user_id.clone(),
        };
        self.display_name = Some(name);
    }

    fn footer(&self) -> CreateEmbedFooter<'_> {
        let name = self
            .display_name
            .as_deref()
            .unwrap_or(self.user_id.as_str());
        CreateEmbedFooter::new(format!("From: @{}", name))
    }

    /// Post a "Receiving fax..." status message. Returns the message ID for future edits.
    pub async fn post_fax_receiving(&mut self) -> Result<u64, FaxError> {
        self.resolve_display_name().await;

        let embed = CreateEmbed::new()
            .title("Incoming Fax")
            .description("Receiving fax...")
            .color(COLOR_RECEIVING)
            .footer(self.footer());

        let msg = self
            .channel_id
            .widen()
            .send_message(&self.http, CreateMessage::new().embed(embed))
            .await?;

        debug!("Posted fax receiving message: {}", msg.id);
        Ok(msg.id.get())
    }

    /// Replace the "Receiving fax..." message with the completed fax and image attachments.
    ///
    /// Deletes the original status message and posts a new one with embeds + images.
    /// Uses one embed per page with a shared URL so Discord renders them as a gallery.
    /// `file_ext` is the file extension without dot (e.g. "png" or "jpg").
    ///
    /// Discord limits messages to 10 embeds. For faxes with >10 pages, the first 10
    /// pages are shown in the embed gallery, and remaining pages are attached as files.
    pub async fn edit_fax_complete(
        &self,
        message_id: u64,
        image_pages: Vec<Vec<u8>>,
        page_count: u32,
        file_ext: &str,
    ) -> Result<(), FaxError> {
        /// Discord's maximum number of embeds per message.
        const MAX_EMBEDS: u32 = 10;

        let embed_count = page_count.min(MAX_EMBEDS);
        let has_overflow = page_count > MAX_EMBEDS;

        let description = if page_count == 1 {
            "Fax received — 1 page".to_string()
        } else if has_overflow {
            format!(
                "Fax received — {} pages (showing first {})",
                page_count, MAX_EMBEDS
            )
        } else {
            format!("Fax received — {} pages", page_count)
        };

        // One embed per page (up to MAX_EMBEDS) with a shared URL for gallery rendering
        let mut embeds = Vec::with_capacity(embed_count as usize);
        for i in 0..embed_count {
            let filename = format!("fax_page_{}.{}", i + 1, file_ext);
            let image_url = format!("attachment://{}", filename);

            let embed = if i == 0 {
                CreateEmbed::new()
                    .title("Fax Received")
                    .description(description.clone())
                    .color(COLOR_COMPLETE)
                    .url(GALLERY_URL)
                    .image(image_url)
                    .footer(self.footer())
            } else {
                CreateEmbed::new()
                    .color(COLOR_COMPLETE)
                    .url(GALLERY_URL)
                    .image(image_url)
            };
            embeds.push(embed);
        }

        // All pages are attached as files (embed pages get rendered in gallery,
        // overflow pages appear as plain file attachments)
        let attachments: Vec<CreateAttachment> = image_pages
            .into_iter()
            .enumerate()
            .map(|(i, data)| {
                CreateAttachment::bytes(data, format!("fax_page_{}.{}", i + 1, file_ext))
            })
            .collect();

        let mut edit = EditMessage::new().embeds(embeds);
        for attachment in attachments {
            edit = edit.new_attachment(attachment);
        }

        if let Err(e) = self
            .channel_id
            .widen()
            .edit_message(&self.http, MessageId::new(message_id), edit)
            .await
        {
            error!(
                "Discord API error editing fax complete (msg={}, {} pages): {}",
                message_id, page_count, e
            );
            return Err(FaxError::Discord(e));
        }

        Ok(())
    }

    /// Edit the status message to show a failure reason.
    pub async fn edit_fax_failed(
        &self,
        message_id: u64,
        reason: &str,
    ) -> Result<(), FaxError> {
        let embed = CreateEmbed::new()
            .title("Fax Failed")
            .description(reason)
            .color(COLOR_FAILED)
            .footer(self.footer());

        if let Err(e) = self
            .channel_id
            .widen()
            .edit_message(
                &self.http,
                MessageId::new(message_id),
                EditMessage::new().embed(embed),
            )
            .await
        {
            error!("Discord API error editing fax failed: {}", e);
        }

        Ok(())
    }

    /// Post a standalone failure message (when no "receiving" message was posted).
    pub async fn post_fax_failed(&mut self, reason: &str) -> Result<(), FaxError> {
        self.resolve_display_name().await;

        let embed = CreateEmbed::new()
            .title("Fax Failed")
            .description(reason)
            .color(COLOR_FAILED)
            .footer(self.footer());

        if let Err(e) = self
            .channel_id
            .widen()
            .send_message(&self.http, CreateMessage::new().embed(embed))
            .await
        {
            error!("Discord API error posting fax failed: {}", e);
        }

        Ok(())
    }
}
