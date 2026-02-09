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
use crate::config::{CmdConfig, Config, DiscordChannelConfig, NostaroConfig};
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

// ─── REST API response types ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DiscordChannelInfo {
    id: String,
    #[serde(rename = "type")]
    channel_type: u8,
    #[serde(default)]
    name: String,
    topic: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordMessageEntry {
    content: String,
    author: MessageAuthor,
    timestamp: String,
}

#[derive(Debug, Deserialize)]
struct ChannelDetail {
    guild_id: Option<String>,
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
        // Per-channel agent map for session persistence
        let agents: Arc<Mutex<HashMap<String, Agent>>> = Arc::new(Mutex::new(HashMap::new()));

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
            Self::process_batch(
                &batch,
                &config,
                &http,
                &token,
                &last_error_sent,
                Arc::clone(&agents),
            )
            .await;
        }
        info!("Queue processor shutting down (channel closed)");
    }

    async fn process_batch(
        batch: &[QueuedMessage],
        config: &Config,
        http: &reqwest::Client,
        token: &str,
        last_error_sent: &std::sync::Mutex<HashMap<String, Instant>>,
        agents: Arc<Mutex<HashMap<String, Agent>>>,
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

        // Generate response using per-channel Agent
        let channel_id_owned = channel_id.clone();
        let config_clone = config.clone();
        let combined = combined_content.clone();
        let agents_init = Arc::clone(&agents);

        let result = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut agents_guard = agents_init.lock().await;

                // Get or create Agent for this channel
                if !agents_guard.contains_key(&channel_id_owned) {
                    let agent_config = AgentCfg {
                        model: config_clone.agent.default_model.clone(),
                        context_window: config_clone.agent.context_window,
                        reserve_tokens: config_clone.agent.reserve_tokens,
                    };
                    let memory = MemoryManager::new_with_full_config(
                        &config_clone.memory,
                        Some(&config_clone),
                        "discord",
                    )?;
                    let mut agent =
                        Agent::new(agent_config, &config_clone, memory).await?;
                    agent.new_session().await?;
                    agents_guard.insert(channel_id_owned.clone(), agent);
                    info!("Created new Agent for channel {}", channel_id_owned);
                }

                let agent = agents_guard.get_mut(&channel_id_owned).unwrap();

                // Check if SOUL.md changed; if so, session reloads automatically
                if let Ok(reloaded) = agent.check_and_reload_soul().await {
                    if reloaded {
                        info!(
                            "SOUL.md changed, session reloaded for channel {}",
                            channel_id_owned
                        );
                    }
                }

                agent.chat(&combined).await
            })
        })
        .await;

        let mut response = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                error!("Failed to generate response: {}", e);
                Self::send_error_if_allowed(http, token, channel_id, last_error_sent).await;
                return;
            }
            Err(e) => {
                error!("Agent task panicked: {}", e);
                Self::send_error_if_allowed(http, token, channel_id, last_error_sent).await;
                return;
            }
        };

        // Tool output loop: process [LIST:...] and [READ:...] tags (max 3 iterations)
        for iteration in 0..3 {
            let tool_output =
                Self::execute_tool_tags(&response, config, http, token).await;
            if tool_output.is_empty() {
                break;
            }
            info!(
                "Tool output loop iteration {} for channel {}",
                iteration + 1,
                channel_id
            );
            let _ = Self::send_typing_static(http, token, channel_id).await;

            let agents_loop = Arc::clone(&agents);
            let ch_id = channel_id.clone();
            let tool_msg = tool_output;

            let loop_result = tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    let mut guard = agents_loop.lock().await;
                    let agent = guard
                        .get_mut(&ch_id)
                        .ok_or_else(|| anyhow::anyhow!("Agent not found for channel"))?;
                    agent.chat(&tool_msg).await
                })
            })
            .await;

            match loop_result {
                Ok(Ok(r)) => response = r,
                Ok(Err(e)) => {
                    error!("Tool output loop error: {}", e);
                    break;
                }
                Err(e) => {
                    error!("Tool output loop task panicked: {}", e);
                    break;
                }
            }
        }

        // --- Process final response tags ---

        // Extract [POST:channel_id] messages for cross-channel posting
        let post_re = Regex::new(r"\[POST:(\d+)\]\s*([^\[]*)").unwrap();
        let mut cross_posts: Vec<(String, String)> = Vec::new();
        for cap in post_re.captures_iter(&response) {
            let target_channel = cap[1].to_string();
            let post_msg = cap[2].trim().to_string();
            if !post_msg.is_empty() {
                cross_posts.push((target_channel, post_msg));
            }
        }

        // Execute [NOSTARO:...] and [CMD:...] tags (fire-and-forget, errors logged only)
        Self::execute_command_tags(&response, &config.nostaro, &config.cmd).await;

        // Remove [POST:...] sections from response text
        let post_remove_re = Regex::new(r"\[POST:\d+\]\s*[^\[]*").unwrap();
        let response_cleaned = post_remove_re.replace_all(&response, "").to_string();

        // Remove [NOSTARO:...] and [CMD:...] tags from response text
        let cmd_remove_re =
            Regex::new(r"\[(NOSTARO|CMD):[^\]]*\]").unwrap();
        let response_cleaned = cmd_remove_re.replace_all(&response_cleaned, "").to_string();

        // Extract [REACT:emoji] tags
        let react_re = Regex::new(r"\[REACT:([^\]]+)\]").unwrap();
        let reactions: Vec<String> = react_re
            .captures_iter(&response_cleaned)
            .map(|c| c[1].to_string())
            .collect();

        // Remove reaction tags and any remaining [LIST:...]/[READ:...] tags
        let text = react_re.replace_all(&response_cleaned, "").to_string();
        let tool_tag_re = Regex::new(r"\[(?:LIST|READ):\d+(?::\d+)?\]").unwrap();
        let text = tool_tag_re.replace_all(&text, "").trim().to_string();

        // Send cross-channel posts (security: only to channels in configured guilds)
        for (target_channel, post_msg) in &cross_posts {
            let allowed = config
                .channels
                .discord
                .as_ref()
                .map(|dc| {
                    dc.guilds
                        .iter()
                        .any(|g| g.channels.is_empty() || g.channels.contains(target_channel))
                })
                .unwrap_or(false);
            if allowed {
                info!(
                    "Cross-posting to channel {}: {}",
                    target_channel,
                    if post_msg.chars().count() > 40 {
                        format!(
                            "{}...",
                            post_msg.chars().take(40).collect::<String>()
                        )
                    } else {
                        post_msg.clone()
                    }
                );
                if let Err(e) =
                    Self::send_message_static(http, token, target_channel, post_msg).await
                {
                    error!("Failed to cross-post to channel {}: {}", target_channel, e);
                }
            } else {
                warn!(
                    "Cross-post to channel {} denied: not in allowed guild channels",
                    target_channel
                );
            }
        }

        // Add reactions to the last message in batch
        for emoji in &reactions {
            if let Err(e) =
                Self::add_reaction_static(http, token, channel_id, last_message_id, emoji).await
            {
                error!("Failed to add reaction {}: {}", emoji, e);
            }
        }

        // Send text reply unless empty or NO_REPLY
        if !text.is_empty() && text != "NO_REPLY" {
            // Check if text is emoji-only (short, no ASCII characters)
            let trimmed = text.trim();
            let is_emoji_only = !trimmed.is_empty()
                && trimmed.len() <= 32
                && trimmed.chars().all(|c| !c.is_ascii() || c == '\u{fe0f}');

            if is_emoji_only {
                // Convert emoji-only text to reaction instead of message
                let first_emoji: String = trimmed
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .take(2) // Most emoji are 1-2 chars (including variation selectors)
                    .collect();
                if !first_emoji.is_empty() {
                    if let Err(e) = Self::add_reaction_static(
                        http,
                        token,
                        channel_id,
                        last_message_id,
                        &first_emoji,
                    )
                    .await
                    {
                        error!("Failed to add emoji-only reaction {}: {}", first_emoji, e);
                    }
                }
            } else if let Err(e) =
                Self::send_message_static(http, token, channel_id, &text).await
            {
                error!("Failed to send Discord message: {}", e);
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

    async fn send_error_if_allowed(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
        last_error_sent: &std::sync::Mutex<HashMap<String, Instant>>,
    ) {
        let should_send = {
            let mut map = last_error_sent.lock().unwrap();
            let now = Instant::now();
            match map.get(channel_id) {
                Some(last) if now.duration_since(*last).as_secs() < ERROR_RATE_LIMIT_SECS => false,
                _ => {
                    map.insert(channel_id.to_string(), now);
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

    // ─── Discord channel tools (LIST/READ) ──────────────────────────

    /// List channels in a guild via REST API
    async fn list_channels_static(
        http: &reqwest::Client,
        token: &str,
        guild_id: &str,
    ) -> Result<String> {
        let url = format!("{}/guilds/{}/channels", DISCORD_API_BASE, guild_id);
        let resp = http
            .get(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord API error {}: {}", status, body);
        }

        let channels: Vec<DiscordChannelInfo> = resp.json().await?;
        // Filter to text channels (0) and announcement channels (5)
        let formatted: Vec<String> = channels
            .iter()
            .filter(|c| c.channel_type == 0 || c.channel_type == 5)
            .map(|c| match c.topic.as_deref().filter(|t| !t.is_empty()) {
                Some(topic) => format!("{} (ID: {}) - {}", c.name, c.id, topic),
                None => format!("{} (ID: {})", c.name, c.id),
            })
            .collect();

        Ok(formatted.join("\n"))
    }

    /// Read recent messages from a channel via REST API
    async fn read_messages_static(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
        limit: u32,
    ) -> Result<String> {
        let limit = limit.clamp(1, 50);
        let url = format!(
            "{}/channels/{}/messages?limit={}",
            DISCORD_API_BASE, channel_id, limit
        );
        let resp = http
            .get(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord API error {}: {}", status, body);
        }

        let mut messages: Vec<DiscordMessageEntry> = resp.json().await?;
        // Discord returns newest first; reverse for chronological order
        messages.reverse();

        let formatted: Vec<String> = messages
            .iter()
            .map(|m| {
                let time = extract_time_from_timestamp(&m.timestamp);
                format!("[{} {}] {}", m.author.username, time, m.content)
            })
            .collect();

        Ok(formatted.join("\n"))
    }

    /// Get a channel's guild_id for security validation
    async fn get_channel_guild_static(
        http: &reqwest::Client,
        token: &str,
        channel_id: &str,
    ) -> Result<String> {
        let url = format!("{}/channels/{}", DISCORD_API_BASE, channel_id);
        let resp = http
            .get(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to get channel info");
        }

        let info: ChannelDetail = resp.json().await?;
        info.guild_id
            .ok_or_else(|| anyhow::anyhow!("Channel has no guild_id (DM channel?)"))
    }

    /// Execute [NOSTARO:...] and [CMD:...] tags found in a response.
    async fn execute_command_tags(response: &str, nostaro_config: &NostaroConfig, cmd_config: &CmdConfig) {
        let tag_re = Regex::new(r"\[(NOSTARO|CMD):([^\]]+)\]").unwrap();
        for cap in tag_re.captures_iter(response) {
            let tag_type = &cap[1];
            let content = &cap[2];

            if tag_type == "NOSTARO" {
                match Self::match_command_template(content, &nostaro_config.commands, Some(&nostaro_config.binary)) {
                    Some(cmd) => Self::run_command(Some(&nostaro_config.config_dir), &cmd).await,
                    None => warn!("Unknown NOSTARO command: {}", content),
                }
            } else {
                match Self::match_command_template(content, &cmd_config.commands, None) {
                    Some(cmd) => Self::run_command(None, &cmd).await,
                    None => warn!("Unknown CMD command: {}", content),
                }
            }
        }
    }

    /// Match tag content against a group's configured patterns and return the expanded command.
    fn match_command_template(
        tag_content: &str,
        patterns: &HashMap<String, String>,
        binary: Option<&str>,
    ) -> Option<String> {
        let tag_parts: Vec<&str> = tag_content.splitn(20, ':').collect();
        for (pattern, template) in patterns {
            let pattern_parts: Vec<&str> = pattern.splitn(20, ':').collect();
            if let Some(bindings) = Self::match_pattern(&pattern_parts, &tag_parts) {
                let mut result = if let Some(bin) = binary {
                    template.replace("{binary}", bin)
                } else {
                    template.clone()
                };
                for (key, value) in &bindings {
                    result = result.replace(&format!("{{{}}}", key), value);
                }
                return Some(result);
            }
        }
        None
    }

    /// Try to match tag parts against a pattern. Returns bound placeholders on success.
    /// The last placeholder in a pattern greedily captures all remaining segments.
    fn match_pattern(pattern_parts: &[&str], tag_parts: &[&str]) -> Option<Vec<(String, String)>> {
        if tag_parts.len() < pattern_parts.len() {
            return None;
        }
        let mut bindings = Vec::new();
        let mut tag_idx = 0;
        for (i, pp) in pattern_parts.iter().enumerate() {
            if tag_idx >= tag_parts.len() {
                return None;
            }
            if pp.starts_with('{') && pp.ends_with('}') {
                let key = &pp[1..pp.len() - 1];
                if i == pattern_parts.len() - 1 {
                    // Last placeholder captures all remaining segments
                    let remaining = tag_parts[tag_idx..].join(":");
                    bindings.push((key.to_string(), remaining));
                } else {
                    bindings.push((key.to_string(), tag_parts[tag_idx].to_string()));
                }
            } else if tag_parts[tag_idx] != *pp {
                return None;
            }
            tag_idx += 1;
        }
        Some(bindings)
    }

    /// Run a command, optionally with config swap.
    /// If config_swap is Some(dir):
    ///   1. Backup ~/.nostaro/config.toml if it exists
    ///   2. Copy dir/config.toml → ~/.nostaro/config.toml
    ///   3. Execute command via sh -c
    ///   4. Restore original or remove copied file
    /// If config_swap is None, just execute the command directly.
    async fn run_command(config_swap: Option<&str>, command: &str) {
        if let Some(config_dir) = config_swap {
            let config_dir_expanded = shellexpand::tilde(config_dir).to_string();
            let nostaro_dir = shellexpand::tilde("~/.nostaro").to_string();
            let target_config = format!("{}/config.toml", nostaro_dir);
            let source_config = format!("{}/config.toml", config_dir_expanded);

            // Check if source config exists
            if !tokio::fs::metadata(&source_config).await.is_ok() {
                error!("Config swap source not found: {}", source_config);
                return;
            }

            // Ensure ~/.nostaro directory exists
            if let Err(e) = tokio::fs::create_dir_all(&nostaro_dir).await {
                error!("Failed to create dir {}: {}", nostaro_dir, e);
                return;
            }

            // Check if original config exists (for backup/restore)
            let original_exists = tokio::fs::metadata(&target_config).await.is_ok();
            let backup_path = format!("{}.localgpt-backup", target_config);

            // Backup original if it exists
            if original_exists {
                if let Err(e) = tokio::fs::copy(&target_config, &backup_path).await {
                    error!("Failed to backup config: {}", e);
                    return;
                }
            }

            // Copy source config to target
            if let Err(e) = tokio::fs::copy(&source_config, &target_config).await {
                error!("Failed to copy config: {}", e);
                if original_exists {
                    let _ = tokio::fs::rename(&backup_path, &target_config).await;
                }
                return;
            }

            info!("Executing command (config swap): {}", command);

            // Execute command via shell
            let result = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .await;

            match result {
                Ok(output) => {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        info!("Command success: {}", stdout.trim());
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        error!(
                            "Command failed (exit {}): {}",
                            output.status,
                            stderr.trim()
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to execute command: {}", e);
                }
            }

            // Restore original config or remove copied file
            if original_exists {
                if let Err(e) = tokio::fs::rename(&backup_path, &target_config).await {
                    error!("Failed to restore config backup: {}", e);
                }
            } else {
                if let Err(e) = tokio::fs::remove_file(&target_config).await {
                    error!("Failed to remove swapped config: {}", e);
                }
            }
        } else {
            // No config swap — just execute directly
            info!("Executing command: {}", command);

            let result = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .await;

            match result {
                Ok(output) => {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        info!("Command success: {}", stdout.trim());
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        error!(
                            "Command failed (exit {}): {}",
                            output.status,
                            stderr.trim()
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to execute command: {}", e);
                }
            }
        }
    }

    /// Execute [LIST:...] and [READ:...] tool tags found in a response.
    /// Returns a tool_output string to feed back to the agent, or empty if no tags found.
    async fn execute_tool_tags(
        response: &str,
        config: &Config,
        http: &reqwest::Client,
        token: &str,
    ) -> String {
        let mut outputs = Vec::new();

        // Parse [LIST:guild_id]
        let list_re = Regex::new(r"\[LIST:(\d+)\]").unwrap();
        for cap in list_re.captures_iter(response) {
            let guild_id = cap[1].to_string();
            let allowed = config
                .channels
                .discord
                .as_ref()
                .map(|dc| dc.guilds.iter().any(|g| g.guild_id == guild_id))
                .unwrap_or(false);

            if allowed {
                match Self::list_channels_static(http, token, &guild_id).await {
                    Ok(result) => {
                        info!("Listed channels for guild {}", guild_id);
                        outputs.push(format!(
                            "<tool_output>\n[LIST:{}] channels:\n{}\n</tool_output>",
                            guild_id, result
                        ));
                    }
                    Err(e) => {
                        error!("Failed to list channels for guild {}: {}", guild_id, e);
                        outputs.push(format!(
                            "<tool_output>\n[LIST:{}] error: {}\n</tool_output>",
                            guild_id, e
                        ));
                    }
                }
            } else {
                warn!("LIST denied for guild {}: not in allowed list", guild_id);
                outputs.push(format!(
                    "<tool_output>\n[LIST:{}] error: guild not in allowed list\n</tool_output>",
                    guild_id
                ));
            }
        }

        // Parse [READ:channel_id] and [READ:channel_id:count]
        let read_re = Regex::new(r"\[READ:(\d+)(?::(\d+))?\]").unwrap();
        for cap in read_re.captures_iter(response) {
            let channel_id = cap[1].to_string();
            let count: u32 = cap
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(10)
                .min(50);

            // Security: verify channel belongs to an allowed guild
            let allowed =
                match Self::get_channel_guild_static(http, token, &channel_id).await {
                    Ok(guild_id) => config
                        .channels
                        .discord
                        .as_ref()
                        .map(|dc| dc.guilds.iter().any(|g| g.guild_id == guild_id))
                        .unwrap_or(false),
                    Err(e) => {
                        warn!(
                            "Could not verify guild for channel {}: {}",
                            channel_id, e
                        );
                        false
                    }
                };

            if allowed {
                match Self::read_messages_static(http, token, &channel_id, count).await {
                    Ok(result) => {
                        info!("Read {} messages from channel {}", count, channel_id);
                        outputs.push(format!(
                            "<tool_output>\n[READ:{}] messages:\n{}\n</tool_output>",
                            channel_id, result
                        ));
                    }
                    Err(e) => {
                        error!(
                            "Failed to read messages from channel {}: {}",
                            channel_id, e
                        );
                        outputs.push(format!(
                            "<tool_output>\n[READ:{}] error: {}\n</tool_output>",
                            channel_id, e
                        ));
                    }
                }
            } else {
                warn!(
                    "READ denied for channel {}: not in allowed guild",
                    channel_id
                );
                outputs.push(format!(
                    "<tool_output>\n[READ:{}] error: channel not in allowed guild\n</tool_output>",
                    channel_id
                ));
            }
        }

        outputs.join("\n\n")
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

/// Extract HH:MM from a Discord ISO 8601 timestamp
fn extract_time_from_timestamp(ts: &str) -> String {
    // Discord timestamp format: "2026-02-09T10:30:00.000000+00:00"
    if let Some(t_pos) = ts.find('T') {
        let time_part = &ts[t_pos + 1..];
        if time_part.len() >= 5 {
            return time_part[..5].to_string();
        }
    }
    "??:??".to_string()
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
