use anyhow::{Context, Result};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{self, Duration};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::agent::{Agent, AgentConfig as AgentCfg};
use crate::config::{Config, DiscordChannelConfig};
use crate::memory::MemoryManager;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

// Gateway opcodes
const OP_DISPATCH: u8 = 0;
const OP_HEARTBEAT: u8 = 1;
const OP_IDENTIFY: u8 = 2;
const OP_RESUME: u8 = 6;
const OP_RECONNECT: u8 = 7;
const OP_INVALID_SESSION: u8 = 9;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

/// Intents: GUILDS (1<<0) + GUILD_MESSAGES (1<<9) + MESSAGE_CONTENT (1<<15)
const INTENTS: u64 = 33280;

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

// ─── Gateway payloads ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GatewayPayload {
    op: u8,
    d: Option<serde_json::Value>,
    s: Option<u64>,
    t: Option<String>,
}

#[derive(Debug, Serialize)]
struct GatewayCommand {
    op: u8,
    d: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct HelloData {
    heartbeat_interval: u64,
}

#[derive(Debug, Deserialize)]
struct ReadyData {
    session_id: String,
    resume_gateway_url: String,
    user: ReadyUser,
}

#[derive(Debug, Deserialize)]
struct ReadyUser {
    id: String,
    username: String,
}

#[derive(Debug, Deserialize)]
struct MessageCreateData {
    id: String,
    channel_id: String,
    guild_id: Option<String>,
    content: String,
    author: MessageAuthor,
    mentions: Option<Vec<MentionUser>>,
}

#[derive(Debug, Deserialize)]
struct MessageAuthor {
    id: String,
    username: String,
    bot: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MentionUser {
    id: String,
}

// ─── Queued message ─────────────────────────────────────────────────

struct QueuedMessage {
    channel_id: String,
    message_id: String,
    author_name: String,
    content: String,
}

// ─── Discord bot ────────────────────────────────────────────────────

struct SessionState {
    sequence: Option<u64>,
    session_id: Option<String>,
    resume_url: Option<String>,
    bot_user_id: Option<String>,
}

/// Rate limit interval for error messages per channel (seconds)
const ERROR_RATE_LIMIT_SECS: u64 = 60;

pub struct DiscordBot {
    config: Config,
    discord_config: DiscordChannelConfig,
    http: Arc<reqwest::Client>,
    /// Tracks last error message time per channel for rate limiting
    last_error_sent: Arc<std::sync::Mutex<HashMap<String, Instant>>>,
    queue_tx: mpsc::Sender<QueuedMessage>,
    queue_rx: Option<mpsc::Receiver<QueuedMessage>>,
}

impl DiscordBot {
    pub fn new(config: Config) -> Result<Self> {
        let discord_config = config
            .channels
            .discord
            .clone()
            .context("Discord channel config is required")?;

        if discord_config.token.is_empty() {
            anyhow::bail!("Discord bot token is empty");
        }

        let (queue_tx, queue_rx) = mpsc::channel(5);

        Ok(Self {
            config,
            discord_config,
            http: Arc::new(reqwest::Client::new()),
            last_error_sent: Arc::new(std::sync::Mutex::new(HashMap::new())),
            queue_tx,
            queue_rx: Some(queue_rx),
        })
    }

    /// Run the bot with automatic reconnect and exponential backoff.
    pub async fn run(&mut self) -> Result<()> {
        // Take the receiver out and spawn the queue processor task
        let queue_rx = self
            .queue_rx
            .take()
            .expect("queue_rx already taken; run() called twice?");
        let config = self.config.clone();
        let http = Arc::clone(&self.http);
        let token = self.discord_config.token.clone();
        let last_error_sent = Arc::clone(&self.last_error_sent);

        let processor_handle = tokio::spawn(async move {
            Self::queue_processor(queue_rx, config, http, token, last_error_sent).await;
        });

        let mut backoff_secs = 1u64;
        let max_backoff = 60u64;
        let mut state = SessionState {
            sequence: None,
            session_id: None,
            resume_url: None,
            bot_user_id: None,
        };

        loop {
            let url = state
                .resume_url
                .as_deref()
                .unwrap_or(GATEWAY_URL)
                .to_string();

            match self.connect_and_run(&url, &mut state).await {
                Ok(()) => {
                    info!("Discord gateway closed normally");
                    break;
                }
                Err(e) => {
                    error!("Discord gateway error: {}", e);
                    info!("Reconnecting in {} seconds...", backoff_secs);
                    time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff);
                }
            }
        }

        processor_handle.abort();
        Ok(())
    }

    /// Batch delay: wait this long after first message to collect more
    const BATCH_DELAY: Duration = Duration::from_secs(3);

    async fn queue_processor(
        mut rx: mpsc::Receiver<QueuedMessage>,
        config: Config,
        http: Arc<reqwest::Client>,
        token: String,
        last_error_sent: Arc<std::sync::Mutex<HashMap<String, Instant>>>,
    ) {
        while let Some(first_msg) = rx.recv().await {
            // Collect batch: wait BATCH_DELAY, gathering any additional messages
            let mut batch = vec![first_msg];
            let deadline = tokio::time::Instant::now() + Self::BATCH_DELAY;

            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(msg)) => batch.push(msg),
                    Ok(None) => {
                        // Channel closed
                        info!("Queue processor shutting down (channel closed)");
                        return;
                    }
                    Err(_) => break, // Timeout reached, process the batch
                }
            }

            info!("Processing batch of {} message(s)", batch.len());
            Self::process_batch(&batch, &config, &http, &token, &last_error_sent).await;
        }
        info!("Queue processor shutting down (channel closed)");
    }

    async fn process_batch(
        batch: &[QueuedMessage],
        config: &Config,
        http: &reqwest::Client,
        token: &str,
        last_error_sent: &std::sync::Mutex<HashMap<String, Instant>>,
    ) {
        if batch.is_empty() {
            return;
        }

        // Use the last message's channel_id and message_id for reactions/replies
        let last_msg = batch.last().unwrap();
        let channel_id = &last_msg.channel_id;
        let last_message_id = &last_msg.message_id;

        // Build combined prompt: format each message as [author] content
        let combined_content = if batch.len() == 1 {
            batch[0].content.clone()
        } else {
            batch
                .iter()
                .map(|m| format!("[{}] {}", m.author_name, m.content))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Send typing indicator
        let _ = Self::send_typing_static(http, token, channel_id).await;

        // Generate response via Agent (single call for entire batch)
        match Self::generate_response_static(config, &combined_content).await {
            Ok(response) => {
                // Extract [REACT:emoji] tags
                let react_re = Regex::new(r"\[REACT:([^\]]+)\]").unwrap();
                let reactions: Vec<String> = react_re
                    .captures_iter(&response)
                    .map(|c| c[1].to_string())
                    .collect();

                // Remove reaction tags from response text
                let text = react_re.replace_all(&response, "").trim().to_string();

                // Add reactions to the last message in batch
                for emoji in &reactions {
                    if let Err(e) =
                        Self::add_reaction_static(http, token, channel_id, last_message_id, emoji)
                            .await
                    {
                        error!("Failed to add reaction {}: {}", emoji, e);
                    }
                }

                // Send text reply unless empty or NO_REPLY
                if !text.is_empty() && text != "NO_REPLY" {
                    if let Err(e) =
                        Self::send_message_static(http, token, channel_id, &text).await
                    {
                        error!("Failed to send Discord message: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to generate response: {}", e);
                let should_send = {
                    let mut map = last_error_sent.lock().unwrap();
                    let now = Instant::now();
                    match map.get(channel_id) {
                        Some(last)
                            if now.duration_since(*last).as_secs() < ERROR_RATE_LIMIT_SECS =>
                        {
                            false
                        }
                        _ => {
                            map.insert(channel_id.clone(), now);
                            true
                        }
                    }
                };
                if should_send {
                    let _ = Self::send_message_static(
                        http,
                        token,
                        channel_id,
                        "Sorry, I encountered an error.",
                    )
                    .await;
                } else {
                    debug!(
                        "Suppressed error message to channel {} (rate limited)",
                        channel_id
                    );
                }
            }
        }
    }

    async fn connect_and_run(&self, url: &str, state: &mut SessionState) -> Result<()> {
        let (ws, _) = connect_async(url)
            .await
            .context("Failed to connect to Discord gateway")?;
        info!("Connected to Discord gateway");

        let (sink, stream) = ws.split();
        let sink = Arc::new(Mutex::new(sink));

        let mut stream = stream;

        // Wait for HELLO
        let heartbeat_interval = self.wait_for_hello(&mut stream).await?;
        info!(
            "Received HELLO, heartbeat interval: {}ms",
            heartbeat_interval
        );

        // Send IDENTIFY or RESUME
        if let Some(ref sid) = state.session_id {
            if let Some(seq) = state.sequence {
                self.send_resume(&sink, sid, seq).await?;
                info!("Sent RESUME for session {}", sid);
            } else {
                self.send_identify(&sink).await?;
                info!("Sent IDENTIFY");
            }
        } else {
            self.send_identify(&sink).await?;
            info!("Sent IDENTIFY");
        }

        // Spawn heartbeat task
        let hb_sink = Arc::clone(&sink);
        let hb_interval = heartbeat_interval;
        let heartbeat_handle = tokio::spawn(async move {
            Self::heartbeat_loop(hb_sink, hb_interval).await;
        });

        // Event loop
        let result = self.event_loop(&mut stream, &sink, state).await;

        heartbeat_handle.abort();
        result
    }

    async fn wait_for_hello(&self, stream: &mut WsStream) -> Result<u64> {
        while let Some(msg) = stream.next().await {
            let msg = msg?;
            if let WsMessage::Text(text) = msg {
                let payload: GatewayPayload = serde_json::from_str(&text)?;
                if payload.op == OP_HELLO {
                    let hello: HelloData = serde_json::from_value(
                        payload.d.context("HELLO payload missing data")?,
                    )?;
                    return Ok(hello.heartbeat_interval);
                }
            }
        }
        anyhow::bail!("Gateway closed before sending HELLO")
    }

    async fn send_identify(&self, sink: &Arc<Mutex<WsSink>>) -> Result<()> {
        let identify = GatewayCommand {
            op: OP_IDENTIFY,
            d: serde_json::json!({
                "token": self.discord_config.token,
                "intents": INTENTS,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "localgpt",
                    "device": "localgpt"
                }
            }),
        };
        let text = serde_json::to_string(&identify)?;
        sink.lock().await.send(WsMessage::Text(text)).await?;
        Ok(())
    }

    async fn send_resume(
        &self,
        sink: &Arc<Mutex<WsSink>>,
        session_id: &str,
        sequence: u64,
    ) -> Result<()> {
        let resume = GatewayCommand {
            op: OP_RESUME,
            d: serde_json::json!({
                "token": self.discord_config.token,
                "session_id": session_id,
                "seq": sequence
            }),
        };
        let text = serde_json::to_string(&resume)?;
        sink.lock().await.send(WsMessage::Text(text)).await?;
        Ok(())
    }

    async fn heartbeat_loop(sink: Arc<Mutex<WsSink>>, interval_ms: u64) {
        // Jitter: first heartbeat at interval * random(0..1), then every interval
        let jitter_ms = interval_ms / 2;
        time::sleep(Duration::from_millis(jitter_ms)).await;

        let mut ticker = time::interval(Duration::from_millis(interval_ms));
        loop {
            ticker.tick().await;
            let hb = serde_json::json!({"op": OP_HEARTBEAT, "d": null});
            let text = serde_json::to_string(&hb).unwrap();
            if let Err(e) = sink.lock().await.send(WsMessage::Text(text)).await {
                warn!("Failed to send heartbeat: {}", e);
                break;
            }
            debug!("Sent heartbeat");
        }
    }

    async fn event_loop(
        &self,
        stream: &mut WsStream,
        sink: &Arc<Mutex<WsSink>>,
        state: &mut SessionState,
    ) -> Result<()> {
        while let Some(msg) = stream.next().await {
            let msg = msg?;
            match msg {
                WsMessage::Text(text) => {
                    let payload: GatewayPayload = serde_json::from_str(&text)?;

                    // Update sequence
                    if let Some(s) = payload.s {
                        state.sequence = Some(s);
                    }

                    match payload.op {
                        OP_DISPATCH => {
                            if let Some(ref event_name) = payload.t {
                                self.handle_dispatch(event_name, payload.d, state).await;
                            }
                        }
                        OP_HEARTBEAT => {
                            // Server requesting immediate heartbeat
                            let hb = serde_json::json!({"op": OP_HEARTBEAT, "d": state.sequence});
                            let text = serde_json::to_string(&hb)?;
                            sink.lock().await.send(WsMessage::Text(text)).await?;
                        }
                        OP_RECONNECT => {
                            info!("Received RECONNECT, will reconnect");
                            return Err(anyhow::anyhow!("Server requested reconnect"));
                        }
                        OP_INVALID_SESSION => {
                            let resumable = payload
                                .d
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            if !resumable {
                                info!("Invalid session (not resumable), resetting state");
                                state.session_id = None;
                                state.sequence = None;
                            }
                            return Err(anyhow::anyhow!("Invalid session"));
                        }
                        OP_HEARTBEAT_ACK => {
                            debug!("Heartbeat ACK received");
                        }
                        _ => {
                            debug!("Unhandled opcode: {}", payload.op);
                        }
                    }
                }
                WsMessage::Close(frame) => {
                    info!("WebSocket closed: {:?}", frame);
                    return Err(anyhow::anyhow!("WebSocket closed"));
                }
                _ => {}
            }
        }

        Err(anyhow::anyhow!("Stream ended"))
    }

    async fn handle_dispatch(
        &self,
        event_name: &str,
        data: Option<serde_json::Value>,
        state: &mut SessionState,
    ) {
        match event_name {
            "READY" => {
                if let Some(d) = data {
                    match serde_json::from_value::<ReadyData>(d) {
                        Ok(ready) => {
                            info!(
                                "READY: logged in as {} ({})",
                                ready.user.username, ready.user.id
                            );
                            state.session_id = Some(ready.session_id);
                            state.resume_url = Some(ready.resume_gateway_url);
                            state.bot_user_id = Some(ready.user.id);
                        }
                        Err(e) => error!("Failed to parse READY: {}", e),
                    }
                }
            }
            "MESSAGE_CREATE" => {
                if let Some(d) = data {
                    match serde_json::from_value::<MessageCreateData>(d) {
                        Ok(msg) => {
                            self.handle_message_create(&msg, state).await;
                        }
                        Err(e) => error!("Failed to parse MESSAGE_CREATE: {}", e),
                    }
                }
            }
            "RESUMED" => {
                info!("Session resumed successfully");
            }
            _ => {
                debug!("Unhandled event: {}", event_name);
            }
        }
    }

    async fn handle_message_create(&self, msg: &MessageCreateData, state: &SessionState) {
        // Ignore messages from bots (unless allow_bots is set)
        if msg.author.bot.unwrap_or(false) && !self.discord_config.allow_bots {
            return;
        }

        // Ignore messages from ourselves
        if let Some(ref bot_id) = state.bot_user_id {
            if msg.author.id == *bot_id {
                return;
            }
        }

        // Check guild allow-list
        if !self.discord_config.guilds.is_empty() {
            let guild_id = match &msg.guild_id {
                Some(id) => id,
                None => return, // DM - skip if guilds are configured
            };

            let guild_config = self
                .discord_config
                .guilds
                .iter()
                .find(|g| g.guild_id == *guild_id);

            match guild_config {
                None => return, // Guild not in allow-list
                Some(gc) => {
                    // Check channel filter
                    if !gc.channels.is_empty() && !gc.channels.contains(&msg.channel_id) {
                        return;
                    }

                    // Check require_mention
                    if gc.require_mention {
                        let mentioned = msg
                            .mentions
                            .as_ref()
                            .map(|ms| {
                                ms.iter().any(|m| {
                                    state
                                        .bot_user_id
                                        .as_ref()
                                        .map(|bid| m.id == *bid)
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false);
                        if !mentioned {
                            return;
                        }
                    }
                }
            }
        }

        // Skip empty messages
        let content = msg.content.trim();
        if content.is_empty() {
            return;
        }

        // Strip bot mention prefix from content
        let cleaned = self.strip_mention(content, state);

        info!(
            "Message from {} in channel {}: {}",
            msg.author.username,
            msg.channel_id,
            if cleaned.len() > 80 {
                let truncated: String = cleaned.chars().take(40).collect();
                format!("{}...", truncated)
            } else {
                cleaned.clone()
            }
        );

        // Enqueue message for processing (non-blocking)
        let queued = QueuedMessage {
            channel_id: msg.channel_id.clone(),
            message_id: msg.id.clone(),
            author_name: msg.author.username.clone(),
            content: cleaned,
        };

        match self.queue_tx.try_send(queued) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(queued)) => {
                warn!("Message queue full, dropping oldest message");
                // Drain one to make room, then send
                let _ = self.queue_tx.try_send(queued).is_ok();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("Message queue closed unexpectedly");
            }
        }
    }

    fn strip_mention(&self, content: &str, state: &SessionState) -> String {
        if let Some(ref bot_id) = state.bot_user_id {
            let mention = format!("<@{}>", bot_id);
            let mention_nick = format!("<@!{}>", bot_id);
            content
                .replace(&mention, "")
                .replace(&mention_nick, "")
                .trim()
                .to_string()
        } else {
            content.to_string()
        }
    }

    // ─── Static methods used by queue processor ──────────────────

    fn generate_response_static(
        config: &Config,
        message: &str,
    ) -> impl std::future::Future<Output = Result<String>> {
        let config = config.clone();
        let message = message.to_string();

        async move {
            // Agent is not Send+Sync due to SQLite - run in blocking task
            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    let agent_config = AgentCfg {
                        model: config.agent.default_model.clone(),
                        context_window: config.agent.context_window,
                        reserve_tokens: config.agent.reserve_tokens,
                    };

                    let memory = MemoryManager::new_with_full_config(
                        &config.memory,
                        Some(&config),
                        "discord",
                    )?;
                    let mut agent = Agent::new(agent_config, &config, memory).await?;
                    agent.new_session().await?;
                    agent.chat(&message).await
                })
            })
            .await?
        }
    }

    // ─── Discord REST API helpers ───────────────────────────────────

    async fn add_reaction_static(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "{}/channels/{}/messages/{}/reactions/{}/@me",
            DISCORD_API_BASE, channel_id, message_id, encoded_emoji
        );
        let resp = http
            .put(&url)
            .header("Authorization", format!("Bot {}", token))
            .header("Content-Length", "0")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!("Discord reaction API error {}: {}", status, body);
            anyhow::bail!("Failed to add reaction: {}", status);
        }

        Ok(())
    }

    async fn send_message_static(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
        content: &str,
    ) -> Result<()> {
        // Discord message limit is 2000 characters; split if needed
        let chunks = split_message(content, 2000);

        for chunk in chunks {
            let url = format!("{}/channels/{}/messages", DISCORD_API_BASE, channel_id);
            let resp = http
                .post(&url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": chunk}))
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!("Discord API error {}: {}", status, body);
            }
        }

        Ok(())
    }

    async fn send_typing_static(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
    ) -> Result<()> {
        let url = format!("{}/channels/{}/typing", DISCORD_API_BASE, channel_id);
        let _ = http
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;
        Ok(())
    }
}

/// Split a message into chunks respecting the Discord character limit.
/// Tries to split at newline boundaries when possible.
fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if content.len() <= max_len {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        // Try to find a newline to split at (char-boundary safe)
        let byte_max = remaining.char_indices()
            .take_while(|(i, _)| *i < max_len)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(remaining.len().min(max_len));
        let safe_slice = &remaining[..byte_max];
        let split_at = safe_slice
            .rfind('\n')
            .unwrap_or(byte_max);

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest.trim_start_matches('\n');
    }

    chunks
}

/// Start the Discord bot as a background task.
/// Returns the JoinHandle so the caller can abort it on shutdown.
pub async fn start(config: &Config) -> Result<tokio::task::JoinHandle<()>> {
    let mut bot = DiscordBot::new(config.clone())?;
    info!("Starting Discord bot");

    let handle = tokio::spawn(async move {
        if let Err(e) = bot.run().await {
            error!("Discord bot exited with error: {}", e);
        }
    });

    Ok(handle)
}
