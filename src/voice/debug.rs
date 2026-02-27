//! Debug mode state for voice pipeline.
//!
//! When debug mode is enabled (via `debug_on` tool call by the owner user),
//! STT recognition results and LLM response text are posted to a Discord
//! text channel for inspection.
//!
//! ## Channel selection
//! 1. `debug_channel_id` from config (explicit manual override)
//! 2. The VC channel ID itself — Discord voice channels (type 2) support text
//!    messages via POST /channels/{id}/messages ("Text in Voice" feature, added 2022).
//!    This is confirmed by the presence of `last_message_id` in the GUILD_VOICE
//!    channel object in the Discord API docs.

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Shared debug state for the voice pipeline.
#[derive(Clone)]
pub struct DebugState {
    /// Whether debug posting is currently enabled.
    enabled: Arc<Mutex<bool>>,
    /// Reqwest HTTP client for posting to Discord.
    http: Arc<reqwest::Client>,
    /// Bot token for Authorization header.
    discord_token: String,
    /// Channel to post debug messages to.
    /// Priority: config.debug_channel_id → VC channel ID (Text in Voice).
    channel_id: String,
}

impl DebugState {
    /// Create a new DebugState.
    ///
    /// `channel_id` should be `config.debug_channel_id` if set, or the VC channel ID otherwise.
    pub fn new(http: Arc<reqwest::Client>, discord_token: String, channel_id: String) -> Self {
        Self {
            enabled: Arc::new(Mutex::new(false)),
            http,
            discord_token,
            channel_id,
        }
    }

    /// Returns a clone of just the enabled flag (for sharing with tools).
    pub fn enabled_flag(&self) -> Arc<Mutex<bool>> {
        Arc::clone(&self.enabled)
    }

    /// Returns true if debug mode is currently on.
    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    /// Enable debug mode.
    pub async fn enable(&self) {
        *self.enabled.lock().await = true;
        debug!("Debug mode enabled");
    }

    /// Disable debug mode.
    pub async fn disable(&self) {
        *self.enabled.lock().await = false;
        debug!("Debug mode disabled");
    }

    /// Post a debug message to Discord if debug mode is on.
    /// Silently ignores errors to avoid disrupting the voice pipeline.
    pub async fn post_if_enabled(&self, message: &str) {
        if !self.is_enabled().await {
            return;
        }
        self.post(message).await;
    }

    /// Post a message to the debug channel unconditionally.
    pub async fn post(&self, message: &str) {
        let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, self.channel_id);
        let body = serde_json::json!({ "content": message });
        match self
            .http
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.discord_token),
            )
            .json(&body)
            .send()
            .await
        {
            Ok(resp) if !resp.status().is_success() => {
                warn!(
                    status = %resp.status(),
                    channel_id = %self.channel_id,
                    "Debug post to Discord failed"
                );
            }
            Err(e) => {
                warn!("Debug Discord post error: {}", e);
            }
            _ => {}
        }
    }
}
