//! Discord message poster for fax sessions using serenity's HTTP client.
//!
//! Posts embed messages through the fax lifecycle:
//! - "Receiving fax..." (blurple) when negotiation starts
//! - Replaced with "Fax Received" (green) with page image gallery on success
//! - Replaced with "Fax Incomplete" (amber) when useful partial data was recovered
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
use std::time::Duration;
use tracing::{debug, error, warn};

const COLOR_RECEIVING: u32 = 0x5865F2; // Discord blurple
const COLOR_COMPLETE: u32 = 0x57F287; // Green
const COLOR_INCOMPLETE: u32 = 0xF0B232; // Amber
const COLOR_FAILED: u32 = 0xED4245; // Red
const GALLERY_URL: &str = "https://sipcord.net/fax";
pub(crate) const MAX_FAX_PAGES: usize = 10;
const UPLOAD_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

/// Retry the same message edit twice. Replacing attachments makes an uncertain
/// response safe to retry without appending duplicate pages or messages.
async fn retry_upload<F, Fut, T, E>(mut upload: F, delays: [Duration; 2]) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    for delay in delays {
        match upload().await {
            Ok(result) => return Ok(result),
            Err(error) => {
                warn!(%error, retry_delay_secs = delay.as_secs(), "Fax upload failed; retrying")
            }
        }
        tokio::time::sleep(delay).await;
    }
    upload().await
}

/// An encoded page ready to attach to Discord. `page_number` is the original
/// TIFF page number, so gaps remain visible when an unreadable page is omitted.
pub(crate) struct FaxPageAttachment {
    pub(crate) page_number: usize,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Copy)]
enum FaxPostKind {
    Complete,
    Incomplete,
}

struct FaxPresentation {
    title: &'static str,
    description: String,
    color: u32,
}

fn fax_presentation(kind: FaxPostKind, page_count: u32, has_overflow: bool) -> FaxPresentation {
    match kind {
        FaxPostKind::Complete => {
            let description = if page_count == 1 {
                "Fax received — 1 page".to_string()
            } else if has_overflow {
                format!(
                    "Fax received — {page_count} pages. First 10 pages shown; remaining pages truncated."
                )
            } else {
                format!("Fax received — {page_count} pages")
            };
            FaxPresentation {
                title: "Fax Received",
                description,
                color: COLOR_COMPLETE,
            }
        }
        FaxPostKind::Incomplete => {
            let pages = if page_count == 1 { "page" } else { "pages" };
            let overflow = if has_overflow {
                ". First 10 pages shown; remaining pages truncated"
            } else {
                ""
            };
            FaxPresentation {
                title: "Fax Incomplete",
                description: format!(
                    "Fax transmission incomplete — recovered {page_count} {pages}{overflow}. \
                     Some content may be missing; partial pages are shown as received."
                ),
                color: COLOR_INCOMPLETE,
            }
        }
    }
}

fn fax_page_filename(page_number: usize, file_ext: &str) -> String {
    format!("fax_page_{page_number}.{file_ext}")
}

fn fax_result_edit<'a>(
    image_pages: Vec<FaxPageAttachment>,
    page_count: u32,
    file_ext: &str,
    kind: FaxPostKind,
    footer: CreateEmbedFooter<'a>,
) -> EditMessage<'a> {
    let embed_count = image_pages.len().min(MAX_FAX_PAGES);
    let has_overflow = page_count as usize > MAX_FAX_PAGES;
    let presentation = fax_presentation(kind, page_count, has_overflow);

    // One embed per page (up to MAX_FAX_PAGES) with a shared URL for gallery rendering
    let mut embeds = Vec::with_capacity(embed_count);
    for (index, page) in image_pages.iter().take(embed_count).enumerate() {
        let filename = fax_page_filename(page.page_number, file_ext);
        let image_url = format!("attachment://{}", filename);

        let embed = if index == 0 {
            CreateEmbed::new()
                .title(presentation.title)
                .description(presentation.description.clone())
                .color(presentation.color)
                .url(GALLERY_URL)
                .image(image_url)
                .footer(footer.clone())
        } else {
            CreateEmbed::new()
                .color(presentation.color)
                .url(GALLERY_URL)
                .image(image_url)
        };
        embeds.push(embed);
    }

    // The attachment limit applies independently of the embed limit.
    let attachments: Vec<CreateAttachment> = image_pages
        .into_iter()
        .take(MAX_FAX_PAGES)
        .map(|page| {
            CreateAttachment::bytes(page.data, fax_page_filename(page.page_number, file_ext))
        })
        .collect();

    let mut edit = EditMessage::new().remove_all_attachments().embeds(embeds);
    for attachment in attachments {
        edit = edit.new_attachment(attachment);
    }

    edit
}

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
    /// Edits the original status message with embeds and images.
    /// Uses one embed per page with a shared URL so Discord renders them as a gallery.
    /// `file_ext` is the file extension without dot (e.g. "png" or "jpg").
    ///
    /// At most 10 pages are attached, with an explicit notice when more were received.
    pub async fn edit_fax_complete(
        &self,
        message_id: u64,
        image_pages: Vec<Vec<u8>>,
        page_count: u32,
        file_ext: &str,
    ) -> Result<(), FaxError> {
        let image_pages = image_pages
            .into_iter()
            .enumerate()
            .map(|(index, data)| FaxPageAttachment {
                page_number: index + 1,
                data,
            })
            .collect();
        self.edit_fax_result(
            message_id,
            image_pages,
            page_count,
            file_ext,
            FaxPostKind::Complete,
        )
        .await
    }

    /// Replace the status message with the first 10 useful pages recovered from
    /// an incomplete transfer, reporting the total recovered count.
    pub(crate) async fn edit_fax_incomplete(
        &self,
        message_id: u64,
        image_pages: Vec<FaxPageAttachment>,
        page_count: u32,
        file_ext: &str,
    ) -> Result<(), FaxError> {
        self.edit_fax_result(
            message_id,
            image_pages,
            page_count,
            file_ext,
            FaxPostKind::Incomplete,
        )
        .await
    }

    async fn edit_fax_result(
        &self,
        message_id: u64,
        image_pages: Vec<FaxPageAttachment>,
        page_count: u32,
        file_ext: &str,
        kind: FaxPostKind,
    ) -> Result<(), FaxError> {
        let edit = fax_result_edit(image_pages, page_count, file_ext, kind, self.footer());

        if let Err(e) = retry_upload(
            || {
                self.channel_id.widen().edit_message(
                    &self.http,
                    MessageId::new(message_id),
                    edit.clone(),
                )
            },
            UPLOAD_RETRY_DELAYS,
        )
        .await
        {
            error!(
                "Fax upload failed after 3 attempts (msg={}, {} received pages): {}",
                message_id, page_count, e
            );
            return Err(FaxError::Discord(e));
        }

        Ok(())
    }

    /// Edit the status message to show a failure reason.
    pub async fn edit_fax_failed(&self, message_id: u64, reason: &str) -> Result<(), FaxError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upload_retries_twice_and_stops_on_success() {
        for failures in [0, 1, 2, 3] {
            let mut attempts = 0;
            let result = retry_upload(
                || {
                    attempts += 1;
                    std::future::ready(if attempts <= failures {
                        Err("upload rejected")
                    } else {
                        Ok(())
                    })
                },
                [Duration::ZERO; 2],
            )
            .await;
            assert_eq!(attempts, (failures + 1).min(3));
            assert_eq!(result.is_err(), failures == 3);
        }
    }

    #[test]
    fn result_payload_caps_attachments_and_embeds_and_reports_total() {
        for kind in [FaxPostKind::Complete, FaxPostKind::Incomplete] {
            for count in [1, 10, 11, 49] {
                let pages = (1..=count)
                    .map(|page_number| FaxPageAttachment {
                        page_number,
                        data: vec![42],
                    })
                    .collect();
                let edit = fax_result_edit(
                    pages,
                    count as u32,
                    "png",
                    kind,
                    CreateEmbedFooter::new("From: test"),
                );
                let payload = serde_json::to_value(edit).unwrap();
                let attachments = payload["attachments"].as_array().unwrap();
                let embeds = payload["embeds"].as_array().unwrap();
                assert_eq!(attachments.len(), count.min(10));
                assert_eq!(embeds.len(), count.min(10));
                for (index, attachment) in attachments.iter().enumerate() {
                    assert_eq!(
                        attachment["filename"],
                        format!("fax_page_{}.png", index + 1)
                    );
                    assert_eq!(
                        embeds[index]["image"]["url"],
                        format!("attachment://fax_page_{}.png", index + 1)
                    );
                }
                let description = embeds[0]["description"].as_str().unwrap();
                assert!(description.contains(&format!("{count} page")));
                assert_eq!(
                    description.contains("First 10 pages shown; remaining pages truncated"),
                    count > 10
                );
            }
        }
    }

    #[test]
    fn already_capped_pages_still_report_total_received() {
        let pages = (1..=10)
            .map(|page_number| FaxPageAttachment {
                page_number,
                data: vec![42],
            })
            .collect();
        let payload = serde_json::to_value(fax_result_edit(
            pages,
            49,
            "png",
            FaxPostKind::Incomplete,
            CreateEmbedFooter::new("From: test"),
        ))
        .unwrap();
        let description = payload["embeds"][0]["description"].as_str().unwrap();
        assert!(description.contains("recovered 49 pages"));
        assert!(description.contains("remaining pages truncated"));
    }

    #[test]
    fn complete_presentation_is_unchanged() {
        let presentation = fax_presentation(FaxPostKind::Complete, 1, false);
        assert_eq!(presentation.title, "Fax Received");
        assert_eq!(presentation.description, "Fax received — 1 page");
        assert_eq!(presentation.color, COLOR_COMPLETE);
    }

    #[test]
    fn incomplete_presentation_is_amber_and_warns_about_missing_content() {
        let presentation = fax_presentation(FaxPostKind::Incomplete, 2, false);
        assert_eq!(presentation.title, "Fax Incomplete");
        assert!(presentation.description.contains("recovered 2 pages"));
        assert!(presentation.description.contains("content may be missing"));
        assert_eq!(presentation.color, COLOR_INCOMPLETE);
    }

    #[test]
    fn incomplete_presentation_reports_gallery_overflow() {
        let presentation = fax_presentation(FaxPostKind::Incomplete, 12, true);
        assert!(
            presentation
                .description
                .contains("First 10 pages shown; remaining pages truncated")
        );
    }

    #[test]
    fn attachment_filename_preserves_original_page_number() {
        assert_eq!(fax_page_filename(3, "png"), "fax_page_3.png");
    }
}
