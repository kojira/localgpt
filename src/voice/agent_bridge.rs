//! Voice ↔ Agent bridge.
//!
//! Provides direct access to [`crate::agent::Agent`] and
//! [`crate::memory`] without going through the HTTP API,
//! eliminating network round-trip latency.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;
use std::pin::Pin;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{info, warn};

use crate::agent::{Agent, AgentConfig as AgentCfg};
use crate::config::Config;
use crate::memory::MemoryManager;

/// One utterance in a room: (user_id, user_name, text, speech_start_at).
///
/// `speech_start_at` is the `Instant` of the first audible PCM chunk for this
/// utterance, captured in `PipelineWorker` before the STT `SpeechStart` event.
/// It is carried through to `run_room_collector` so the `ProfileSession` can
/// be anchored to the true moment the user began speaking rather than to the
/// (much later) time the message arrived in the room buffer.
pub type RoomMessage = (u64, String, String, std::time::Instant);

/// Bridges voice pipeline workers to the LLM agent.
#[async_trait]
pub trait AgentBridge: Send + Sync {
    /// Generate a text response for a voice user (1:1 mode).
    async fn generate(&self, user_id: u64, text: &str) -> Result<String>;

    /// Stream a text response token-by-token for a voice user.
    ///
    /// Returns a `Stream<Item = Result<String>>` where each item is one token
    /// (or a small chunk) of the LLM's response.  The stream ends naturally
    /// when the LLM finishes or when the sender is dropped (e.g. on cancel).
    async fn generate_stream(
        &self,
        user_id: u64,
        text: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>>;

    /// Reset the conversation context for a user.
    async fn reset_context(&self, user_id: u64) -> Result<()>;

    /// Generate one response for a room from multiple users' utterances.
    /// Messages are (user_id, user_name, text). The LLM sees who said what and replies once.
    async fn generate_room(&self, room_id: u64, messages: &[RoomMessage]) -> Result<String>;

    /// Stream a room response token-by-token.
    ///
    /// Like `generate_stream` but for room mode: formats all messages as a single
    /// combined prompt and streams the LLM reply.
    async fn generate_room_stream(
        &self,
        room_id: u64,
        messages: &[RoomMessage],
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>>;
}

/// Request sent to the agent worker thread.
enum AgentRequest {
    Generate(String, oneshot::Sender<Result<String>>),
    /// Stream tokens back via the unbounded sender; sender is dropped when done/error.
    GenerateStream(String, mpsc::UnboundedSender<Result<String>>),
    Reset(oneshot::Sender<Result<()>>),
    GenerateRoom(u64, Vec<RoomMessage>, oneshot::Sender<Result<String>>),
    /// Stream room response tokens back via the unbounded sender.
    GenerateRoomStream(u64, Vec<RoomMessage>, mpsc::UnboundedSender<Result<String>>),
}

/// Real agent bridge that runs the LLM in a dedicated thread.
///
/// Agent is not `Send`, so we keep one agent per user in a single thread
/// and dispatch generate/reset via a channel.
pub struct RealAgentBridge {
    tx: mpsc::UnboundedSender<(u64, AgentRequest)>,
}

impl RealAgentBridge {
    /// Create a bridge that uses the given config to build agents per user.
    /// Spawns a dedicated thread that owns the agent map and runs the runtime.
    pub fn new(config: Config) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("RealAgentBridge worker runtime failed: {}", e);
                    return;
                }
            };
            rt.block_on(worker_loop(&config, &mut rx));
        });
        Self { tx }
    }
}

/// Key for the agent map: per-user (1:1) or per-room (group).
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum AgentKey {
    User(u64),
    Room(u64),
}

async fn worker_loop(
    config: &Config,
    rx: &mut mpsc::UnboundedReceiver<(u64, AgentRequest)>,
) {
    let mut agents: HashMap<AgentKey, Agent> = HashMap::new();
    while let Some((key_id, req)) = rx.recv().await {
        match req {
            AgentRequest::Generate(text, reply) => {
                let result =
                    get_or_create_then_chat(&mut agents, AgentKey::User(key_id), &text, config).await;
                let _ = reply.send(result);
            }
            AgentRequest::GenerateStream(text, token_tx) => {
                // Drain the LLM stream and forward each token via token_tx.
                // token_tx is dropped at the end so the receiver sees end-of-stream.
                stream_agent_response(&mut agents, AgentKey::User(key_id), &text, config, token_tx).await;
            }
            AgentRequest::Reset(reply) => {
                agents.remove(&AgentKey::User(key_id));
                let _ = reply.send(Ok(()));
            }
            AgentRequest::GenerateRoomStream(room_id, messages, token_tx) => {
                let key = AgentKey::Room(room_id);
                let formatted = format_room_messages(&messages);
                stream_agent_response(&mut agents, key, &formatted, config, token_tx).await;
            }
            AgentRequest::GenerateRoom(room_id, messages, reply) => {
                // #region agent log
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open("/Users/kojira/.openclaw/workspace/projects/localgpt/.cursor/debug.log")
                {
                    use std::io::Write;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let _ = writeln!(
                        f,
                        r#"{{"id":"bridge_room_req","timestamp":{},"location":"voice/agent_bridge.rs","message":"GenerateRoom received","data":{{"room_id":{},"msg_count":{}}},"hypothesisId":"E"}}"#,
                        ts, room_id, messages.len()
                    );
                }
                // #endregion
                let result =
                    get_or_create_room_then_chat(&mut agents, room_id, &messages, config).await;
                // #region agent log
                let is_ok = result.is_ok();
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open("/Users/kojira/.openclaw/workspace/projects/localgpt/.cursor/debug.log")
                {
                    use std::io::Write;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let err_str = result.as_ref().err().map(|e| e.to_string().replace('"', "'")).unwrap_or_default();
                    let _ = writeln!(
                        f,
                        r#"{{"id":"bridge_room_done","timestamp":{},"location":"voice/agent_bridge.rs","message":"get_or_create_room_then_chat done","data":{{"room_id":{},"ok":{},"err":"{}"}},"hypothesisId":"E"}}"#,
                        ts, room_id, is_ok, err_str
                    );
                }
                // #endregion
                let _ = reply.send(result);
            }
        }
    }
    info!("RealAgentBridge worker loop ended");
}

async fn get_or_create_then_chat(
    agents: &mut HashMap<AgentKey, Agent>,
    key: AgentKey,
    text: &str,
    config: &Config,
) -> Result<String> {
    if let Some(agent) = agents.get_mut(&key) {
        return agent.chat(text).await;
    }
    let agent_id = match key {
        AgentKey::User(u) => format!("voice-{}", u),
        AgentKey::Room(r) => format!("voice-room-{}", r),
    };
    let agent_config = AgentCfg {
        model: config.agent.default_model.clone(),
        context_window: config.agent.context_window,
        reserve_tokens: config.agent.reserve_tokens,
    };
    let memory =
        MemoryManager::new_with_full_config(&config.memory, Some(config), &agent_id)?;
    let mut agent = Agent::new(agent_config, config, memory, None).await?;
    agent.new_session().await?;
    let out = agent.chat(text).await;
    if out.is_ok() {
        agents.insert(key, agent);
    }
    out
}

/// Stream LLM tokens for a 1:1 voice user.
///
/// Calls `agent.chat_stream(text)`, drains the resulting `StreamResult`,
/// and forwards each non-empty delta as a `Result<String>` on `token_tx`.
/// After the stream ends, calls `agent.finish_chat_stream` to commit the
/// assistant turn to the conversation context.
/// If the receiver is dropped (cancellation), the function returns early.
async fn stream_agent_response(
    agents: &mut HashMap<AgentKey, Agent>,
    key: AgentKey,
    text: &str,
    config: &Config,
    token_tx: mpsc::UnboundedSender<Result<String>>,
) {
    // Ensure agent exists.
    if !agents.contains_key(&key) {
        let agent_id = match key {
            AgentKey::User(u) => format!("voice-{}", u),
            AgentKey::Room(r) => format!("voice-room-{}", r),
        };
        let agent_config = AgentCfg {
            model: config.agent.default_model.clone(),
            context_window: config.agent.context_window,
            reserve_tokens: config.agent.reserve_tokens,
        };
        let memory = match MemoryManager::new_with_full_config(&config.memory, Some(config), &agent_id) {
            Ok(m) => m,
            Err(e) => {
                let _ = token_tx.send(Err(e));
                return;
            }
        };
        let mut agent = match Agent::new(agent_config, config, memory, None).await {
            Ok(a) => a,
            Err(e) => {
                let _ = token_tx.send(Err(e));
                return;
            }
        };
        if let Err(e) = agent.new_session().await {
            let _ = token_tx.send(Err(e));
            return;
        }
        agents.insert(key, agent);
    }

    let agent = agents.get_mut(&key).unwrap();

    let mut stream = match agent.chat_stream(text).await {
        Ok(s) => s,
        Err(e) => {
            let _ = token_tx.send(Err(e));
            return;
        }
    };

    let mut full_response = String::new();
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                if !chunk.delta.is_empty() {
                    // Strip Claude CLI metadata lines (e.g. "[Model: ... | Tools: N]")
                    // before forwarding to TTS / callers.
                    let filtered = strip_cli_metadata(&chunk.delta);
                    full_response.push_str(&filtered);
                    // Only forward if there is actual content remaining.
                    if !filtered.is_empty() {
                        // If receiver is gone (cancelled), stop streaming.
                        if token_tx.send(Ok(filtered)).is_err() {
                            break;
                        }
                    }
                }
                if chunk.done {
                    break;
                }
            }
            Some(Err(e)) => {
                let _ = token_tx.send(Err(e));
                break;
            }
            None => break,
        }
    }
    // Commit assistant turn to session history so the next turn has context.
    agent.finish_chat_stream(&full_response);
    // token_tx drops here → receiver gets None → stream ends.
}

/// Strip Claude CLI metadata header lines from a streaming text chunk.
///
/// The `ClaudeCliProvider` emits a metadata line as the very first `StreamChunk`
/// when using `stream-json` output format, e.g.:
///
/// ```text
/// [Model: claude-sonnet-4-6 | Tools: 3]
/// ```
///
/// This is useful in a terminal but must **not** be forwarded to TTS because it
/// gets read aloud verbatim.  The function removes any such lines from the chunk
/// and returns the cleaned text (which may be empty if the entire chunk was
/// metadata).
pub fn strip_cli_metadata(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.split('\n') {
        if !is_cli_metadata_line(line) {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Returns `true` if `c` is an emoji or emoji-related character.
pub fn is_emoji_char(c: char) -> bool {
    let cp = c as u32;
    if cp >= 0x1F000 {
        return true;
    }
    matches!(cp,
        0x00A9 | 0x00AE |
        0x203C | 0x2049 |
        0x2122 | 0x2139 |
        0x2194..=0x2199 |
        0x21A9..=0x21AA |
        0x231A..=0x231B |
        0x2328 | 0x23CF |
        0x23E9..=0x23F3 |
        0x23F8..=0x23FA |
        0x24C2 |
        0x25AA..=0x25AB |
        0x25B6 | 0x25C0 |
        0x25FB..=0x25FE |
        0x2600..=0x27BF |
        0x2934..=0x2935 |
        0x2B05..=0x2B07 |
        0x2B1B..=0x2B1C |
        0x2B50 | 0x2B55 |
        0x3030 | 0x303D |
        0x3297 | 0x3299 |
        0xFE00..=0xFE0F |
        0x200D | 0x20E3
    )
}

/// Returns `true` if `line` consists only of emoji characters (and spaces).
pub fn is_emoji_only_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    t.chars().all(|c| is_emoji_char(c) || c == ' ')
}

/// Returns `true` if `line` is a Claude CLI metadata header line that should be
/// suppressed before TTS / downstream consumers see it.
///
/// Matched patterns (after whitespace trimming):
/// * `[Model: …]`  — system-init model info
/// * `[Tools: …]`  — tool-count info
/// * `NO_REPLY`    — OpenClaw agent signal
/// * Emoji-only lines — would be read aloud awkwardly by TTS
fn is_cli_metadata_line(line: &str) -> bool {
    let t = line.trim();
    if (t.starts_with("[Model:") || t.starts_with("[Tools:")) && t.ends_with(']') {
        return true;
    }
    if t == "NO_REPLY" {
        return true;
    }
    if is_emoji_only_line(t) {
        return true;
    }
    false
}

fn format_room_messages(messages: &[RoomMessage]) -> String {
    messages
        .iter()
        .map(|(_id, name, text, _start)| format!("{}: {}", name, text.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn get_or_create_room_then_chat(
    agents: &mut HashMap<AgentKey, Agent>,
    room_id: u64,
    messages: &[RoomMessage],
    config: &Config,
) -> Result<String> {
    let key = AgentKey::Room(room_id);
    let formatted = format_room_messages(messages);
    get_or_create_then_chat(agents, key, &formatted, config).await
}

#[async_trait]
impl AgentBridge for RealAgentBridge {
    async fn generate(&self, user_id: u64, text: &str) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send((user_id, AgentRequest::Generate(text.to_string(), reply_tx)))
            .is_err()
        {
            warn!("RealAgentBridge worker channel closed");
            anyhow::bail!("Agent worker channel closed");
        }
        reply_rx.await.map_err(|e| anyhow::anyhow!("worker reply dropped: {}", e))?
    }

    async fn generate_stream(
        &self,
        user_id: u64,
        text: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        let (token_tx, token_rx) = mpsc::unbounded_channel::<Result<String>>();
        if self
            .tx
            .send((
                user_id,
                AgentRequest::GenerateStream(text.to_string(), token_tx),
            ))
            .is_err()
        {
            warn!("RealAgentBridge worker channel closed");
            anyhow::bail!("Agent worker channel closed");
        }
        // Wrap the receiver as a Stream and return it immediately.
        // The agent worker will stream tokens into it asynchronously.
        let stream = UnboundedReceiverStream::new(token_rx);
        Ok(Box::pin(stream))
    }

    async fn reset_context(&self, user_id: u64) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send((user_id, AgentRequest::Reset(reply_tx)))
            .is_err()
        {
            warn!("RealAgentBridge worker channel closed");
            anyhow::bail!("Agent worker channel closed");
        }
        reply_rx.await.map_err(|e| anyhow::anyhow!("worker reply dropped: {}", e))?
    }

    async fn generate_room(&self, room_id: u64, messages: &[RoomMessage]) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let messages = messages.to_vec();
        if self
            .tx
            .send((
                room_id,
                AgentRequest::GenerateRoom(room_id, messages, reply_tx),
            ))
            .is_err()
        {
            warn!("RealAgentBridge worker channel closed");
            anyhow::bail!("Agent worker channel closed");
        }
        reply_rx.await.map_err(|e| anyhow::anyhow!("worker reply dropped: {}", e))?
    }

    async fn generate_room_stream(
        &self,
        room_id: u64,
        messages: &[RoomMessage],
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        let (token_tx, token_rx) = mpsc::unbounded_channel::<Result<String>>();
        let messages = messages.to_vec();
        if self
            .tx
            .send((
                room_id,
                AgentRequest::GenerateRoomStream(room_id, messages, token_tx),
            ))
            .is_err()
        {
            warn!("RealAgentBridge worker channel closed");
            anyhow::bail!("Agent worker channel closed");
        }
        let stream = UnboundedReceiverStream::new(token_rx);
        Ok(Box::pin(stream))
    }
}

/// Mock agent bridge that echoes user input back.
///
/// Used for testing the pipeline without a real LLM.
pub struct MockAgentBridge;

impl MockAgentBridge {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentBridge for MockAgentBridge {
    async fn generate(&self, _user_id: u64, text: &str) -> Result<String> {
        Ok(format!("echo: {}", text))
    }

    async fn generate_stream(
        &self,
        _user_id: u64,
        text: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        // Return the full response as a single token so tests stay simple.
        let token = format!("echo: {}", text);
        let items: Vec<Result<String>> = vec![Ok(token)];
        Ok(Box::pin(futures::stream::iter(items)))
    }

    async fn reset_context(&self, _user_id: u64) -> Result<()> {
        Ok(())
    }

    async fn generate_room(&self, _room_id: u64, messages: &[RoomMessage]) -> Result<String> {
        let parts: Vec<String> = messages
            .iter()
            .map(|(_, name, text, _)| format!("{}: {}", name, text.trim()))
            .collect();
        Ok(format!("echo: {}", parts.join(" | ")))
    }

    async fn generate_room_stream(
        &self,
        room_id: u64,
        messages: &[RoomMessage],
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>> {
        let token = self.generate_room(room_id, messages).await?;
        let items: Vec<Result<String>> = vec![Ok(token)];
        Ok(Box::pin(futures::stream::iter(items)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_cli_metadata ──────────────────────────────────────────────────

    #[test]
    fn strip_cli_metadata_removes_model_tools_line() {
        let input = "[Model: claude-sonnet-4-6 | Tools: 3]\nほわ〜…「ですと」ってなんか";
        let result = strip_cli_metadata(input);
        // The metadata line is gone; the Japanese text remains.
        assert!(!result.contains("[Model:"), "metadata should be removed");
        assert!(result.contains("ほわ〜"), "actual text must survive");
    }

    #[test]
    fn strip_cli_metadata_standalone_chunk_becomes_empty() {
        // The metadata chunk is just the bracketed line + newline.
        // split('\n') → ["[Model: …]", ""]  → filter → [""] → join → "".
        let input = "[Model: claude-sonnet-4-6 | Tools: 3]\n";
        let result = strip_cli_metadata(input);
        assert!(!result.contains("[Model:"), "metadata line must be gone");
        // The remainder is an empty string; callers treat empty strings as no-ops.
        assert_eq!(result, "");
    }

    #[test]
    fn strip_cli_metadata_tools_only_line() {
        let input = "[Tools: 5]\nSome response";
        let result = strip_cli_metadata(input);
        assert!(!result.contains("[Tools:"));
        assert!(result.contains("Some response"));
    }

    #[test]
    fn strip_cli_metadata_preserves_normal_text() {
        let input = "こんにちは！今日もいい天気ですね。";
        assert_eq!(strip_cli_metadata(input), input);
    }

    #[test]
    fn strip_cli_metadata_preserves_japanese_brackets() {
        // Japanese text with brackets that look superficially similar must NOT be stripped.
        let input = "[笑い] これは本文です";
        assert_eq!(strip_cli_metadata(input), input);
    }

    #[test]
    fn strip_cli_metadata_preserves_non_metadata_bracket_lines() {
        // Lines starting with "[" that aren't "[Model:" or "[Tools:" must be kept.
        let input = "[Note: this is fine]\nActual text";
        let result = strip_cli_metadata(input);
        assert!(result.contains("[Note: this is fine]"));
        assert!(result.contains("Actual text"));
    }

    #[test]
    fn is_cli_metadata_line_model() {
        assert!(is_cli_metadata_line("[Model: claude-sonnet-4-6 | Tools: 3]"));
    }

    #[test]
    fn is_cli_metadata_line_tools() {
        assert!(is_cli_metadata_line("[Tools: 0]"));
    }

    #[test]
    fn is_cli_metadata_line_rejects_normal() {
        assert!(!is_cli_metadata_line("hello"));
        assert!(!is_cli_metadata_line("[笑い]"));
        assert!(!is_cli_metadata_line("[Note: something]"));
        assert!(!is_cli_metadata_line(""));
    }

    #[test]
    fn strip_cli_metadata_removes_no_reply_line() {
        let input = "NO_REPLY\nActual text";
        let result = strip_cli_metadata(input);
        assert!(!result.contains("NO_REPLY"));
        assert!(result.contains("Actual text"));
    }

    #[test]
    fn strip_cli_metadata_removes_emoji_only_line() {
        let input = "🎉\nActual text";
        let result = strip_cli_metadata(input);
        assert!(!result.trim_start().starts_with('🎉'), "emoji-only line should be removed");
        assert!(result.contains("Actual text"));
    }

    #[test]
    fn is_cli_metadata_no_reply() {
        assert!(is_cli_metadata_line("NO_REPLY"));
        assert!(is_cli_metadata_line("  NO_REPLY  "));
    }

    #[test]
    fn is_cli_metadata_emoji_only() {
        assert!(is_cli_metadata_line("🎉"));
        assert!(is_cli_metadata_line("🚀 🎊"));
    }

    #[test]
    fn is_emoji_only_line_tests() {
        assert!(is_emoji_only_line("🎉"));
        assert!(is_emoji_only_line("🚀 🎊"));
        assert!(!is_emoji_only_line("Hello 🎉"));
        assert!(!is_emoji_only_line(""));
        assert!(!is_emoji_only_line("   "));
        assert!(!is_emoji_only_line("NO_REPLY"));
    }

    // ── MockAgentBridge ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn mock_bridge_echoes() {
        let bridge = MockAgentBridge::new();
        let result = bridge.generate(1, "hello").await.unwrap();
        assert_eq!(result, "echo: hello");
    }

    #[tokio::test]
    async fn mock_bridge_reset_ok() {
        let bridge = MockAgentBridge::new();
        assert!(bridge.reset_context(42).await.is_ok());
    }

    #[tokio::test]
    async fn mock_bridge_is_send_sync() {
        let bridge: std::sync::Arc<dyn AgentBridge> = std::sync::Arc::new(MockAgentBridge::new());
        let b = bridge.clone();
        let handle = tokio::spawn(async move { b.generate(1, "test").await });
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, "echo: test");
    }
}
