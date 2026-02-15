//! Voice ↔ Agent bridge.
//!
//! Provides direct access to [`crate::agent::Agent`] and
//! [`crate::memory`] without going through the HTTP API,
//! eliminating network round-trip latency.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::agent::{Agent, AgentConfig as AgentCfg};
use crate::config::Config;
use crate::memory::MemoryManager;

/// One utterance in a room (user_id, user_name, text).
pub type RoomMessage = (u64, String, String);

/// Bridges voice pipeline workers to the LLM agent.
#[async_trait]
pub trait AgentBridge: Send + Sync {
    /// Generate a text response for a voice user (1:1 mode).
    async fn generate(&self, user_id: u64, text: &str) -> Result<String>;

    /// Reset the conversation context for a user.
    async fn reset_context(&self, user_id: u64) -> Result<()>;

    /// Generate one response for a room from multiple users' utterances.
    /// Messages are (user_id, user_name, text). The LLM sees who said what and replies once.
    async fn generate_room(&self, room_id: u64, messages: &[RoomMessage]) -> Result<String>;
}

/// Request sent to the agent worker thread.
enum AgentRequest {
    Generate(String, oneshot::Sender<Result<String>>),
    Reset(oneshot::Sender<Result<()>>),
    GenerateRoom(u64, Vec<RoomMessage>, oneshot::Sender<Result<String>>),
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
            AgentRequest::Reset(reply) => {
                agents.remove(&AgentKey::User(key_id));
                let _ = reply.send(Ok(()));
            }
            AgentRequest::GenerateRoom(room_id, messages, reply) => {
                let result =
                    get_or_create_room_then_chat(&mut agents, room_id, &messages, config).await;
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
    let mut agent = Agent::new(agent_config, config, memory).await?;
    agent.new_session().await?;
    let out = agent.chat(text).await;
    if out.is_ok() {
        agents.insert(key, agent);
    }
    out
}

fn format_room_messages(messages: &[RoomMessage]) -> String {
    messages
        .iter()
        .map(|(_id, name, text)| format!("{}: {}", name, text.trim()))
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

    async fn reset_context(&self, _user_id: u64) -> Result<()> {
        Ok(())
    }

    async fn generate_room(&self, _room_id: u64, messages: &[RoomMessage]) -> Result<String> {
        let parts: Vec<String> = messages
            .iter()
            .map(|(_, name, text)| format!("{}: {}", name, text.trim()))
            .collect();
        Ok(format!("echo: {}", parts.join(" | ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
