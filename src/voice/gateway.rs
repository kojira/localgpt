//! Discord Voice Gateway integration via songbird standalone driver.
//!
//! Manages the songbird `Call` lifecycle without serenity,
//! using `ConnectionInfo` built from raw Gateway events forwarded
//! by the existing text-gateway in `src/discord/`.

use anyhow::{Context, Result};
use dashmap::DashMap;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// VC connection state machine
#[derive(Debug, Clone, PartialEq)]
pub enum VcConnectionState {
    /// Not connected
    Disconnected,

    /// Connecting (Voice State Update sent, waiting for Voice Server Update)
    Connecting {
        started_at: Instant,
        guild_id: u64,
        channel_id: u64,
    },

    /// Connected (audio send/receive ready)
    Connected {
        guild_id: u64,
        channel_id: u64,
        connected_at: Instant,
    },

    /// Reconnecting (auto-recovery after connection loss)
    Reconnecting {
        guild_id: u64,
        channel_id: u64,
        attempt: u32,
        max_attempts: u32,
        last_attempt_at: Instant,
    },
}

impl VcConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Connected { .. } | Self::Reconnecting { .. })
    }

    pub fn guild_id(&self) -> Option<u64> {
        match self {
            Self::Disconnected => None,
            Self::Connecting { guild_id, .. }
            | Self::Connected { guild_id, .. }
            | Self::Reconnecting { guild_id, .. } => Some(*guild_id),
        }
    }
}

/// Voice State Update payload (from Discord Gateway)
#[derive(Debug, Clone)]
pub struct VoiceStateData {
    pub guild_id: u64,
    pub channel_id: Option<u64>,
    pub user_id: u64,
    pub session_id: String,
}

/// Voice Server Update payload (from Discord Gateway)
#[derive(Debug, Clone)]
pub struct VoiceServerData {
    pub guild_id: u64,
    pub token: String,
    pub endpoint: String,
}

/// Wraps songbird connection state and manages VC join/leave with state machine.
pub struct VoiceGateway {
    bot_user_id: u64,
    /// Pending Voice State Update data (waiting for Voice Server Update)
    pending_voice_states: DashMap<u64, VoiceStateData>,
    /// Connection state per guild
    connection_states: DashMap<u64, VcConnectionState>,
}

impl VoiceGateway {
    /// Create a new gateway with the bot's user ID
    pub fn new(bot_user_id: u64) -> Self {
        // TODO: Initialize songbird standalone driver here
        Self {
            bot_user_id,
            pending_voice_states: DashMap::new(),
            connection_states: DashMap::new(),
        }
    }

    /// Join a voice channel (sends Voice State Update via existing gateway)
    pub async fn join(
        &self,
        guild_id: u64,
        channel_id: u64,
        gateway_tx: &mpsc::Sender<serde_json::Value>,
    ) -> Result<()> {

        // Transition: Disconnected → Connecting
        self.transition(
            guild_id,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id,
                channel_id,
            },
        )?;

        // Send Voice State Update (op=4) to Discord Gateway
        let voice_state_update = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.to_string(),
                "channel_id": channel_id.to_string(),
                "self_mute": false,
                "self_deaf": false,
            }
        });

        gateway_tx
            .send(voice_state_update)
            .await
            .context("Failed to send Voice State Update to gateway")?;

        info!(guild_id, channel_id, "Sent Voice State Update");
        Ok(())
    }

    /// Leave a voice channel
    pub async fn leave(&self, guild_id: u64) -> Result<()> {
        // TODO: Remove Call from songbird when driver is initialized

        // Transition: Connected → Disconnected
        self.transition(guild_id, VcConnectionState::Disconnected)?;

        // Clean up pending state
        self.pending_voice_states.remove(&guild_id);

        info!(guild_id, "Left voice channel");
        Ok(())
    }

    /// Handle Voice State Update from Discord Gateway
    pub async fn handle_voice_state_update(&self, data: VoiceStateData) {
        // Ignore updates for other users (we only care about our own bot)
        if data.user_id != self.bot_user_id {
            return;
        }

        // If channel_id is None, the bot was disconnected
        if data.channel_id.is_none() {
            info!(guild_id = data.guild_id, "Bot was disconnected from VC");
            let _ = self.transition(data.guild_id, VcConnectionState::Disconnected);
            self.pending_voice_states.remove(&data.guild_id);
            return;
        }

        // Store session_id for later use with Voice Server Update
        let guild_id = data.guild_id;
        self.pending_voice_states.insert(guild_id, data);
        info!(guild_id, "Voice State Update received");
    }

    /// Handle Voice Server Update from Discord Gateway
    pub async fn handle_voice_server_update(&self, data: VoiceServerData) {
        // Get the corresponding Voice State Update
        let state = match self.pending_voice_states.remove(&data.guild_id) {
            Some((_, state)) => state,
            None => {
                warn!(
                    guild_id = data.guild_id,
                    "Voice Server Update received without prior Voice State Update"
                );
                return;
            }
        };

        let channel_id = match state.channel_id {
            Some(cid) => cid,
            None => {
                warn!(
                    guild_id = data.guild_id,
                    "Voice State has no channel_id"
                );
                return;
            }
        };

        // TODO: Build ConnectionInfo and connect via songbird standalone driver
        // When songbird driver is initialized:
        // 1. Build ConnectionInfo { channel_id, endpoint, guild_id, session_id, token, user_id }
        // 2. Get or create Call via songbird.get_or_insert()
        // 3. Call call.connect(info).await

        // Transition: Connecting → Connected
        if let Err(e) = self.transition(
            data.guild_id,
            VcConnectionState::Connected {
                guild_id: data.guild_id,
                channel_id,
                connected_at: Instant::now(),
            },
        ) {
            error!(guild_id = data.guild_id, error = %e, "State transition failed");
            return;
        }

        info!(
            guild_id = data.guild_id,
            channel_id,
            "Voice server update processed (driver pending)"
        );
    }

    /// Transition to a new state (validates transition)
    fn transition(&self, guild_id: u64, new_state: VcConnectionState) -> Result<()> {
        let current = self
            .connection_states
            .get(&guild_id)
            .map(|r| r.clone())
            .unwrap_or(VcConnectionState::Disconnected);

        // Validate state transition
        let valid = match (&current, &new_state) {
            (VcConnectionState::Disconnected, VcConnectionState::Connecting { .. }) => true,
            (VcConnectionState::Connecting { .. }, VcConnectionState::Connected { .. }) => true,
            (VcConnectionState::Connecting { .. }, VcConnectionState::Disconnected) => true, // timeout
            (VcConnectionState::Connected { .. }, VcConnectionState::Disconnected) => true, // leave
            (VcConnectionState::Connected { .. }, VcConnectionState::Reconnecting { .. }) => true,
            (VcConnectionState::Reconnecting { .. }, VcConnectionState::Connected { .. }) => true,
            (VcConnectionState::Reconnecting { .. }, VcConnectionState::Disconnected) => true, // give up
            _ => false,
        };

        if !valid {
            anyhow::bail!(
                "Invalid VC state transition: {:?} → {:?}",
                current,
                new_state
            );
        }

        info!(
            guild_id,
            from = ?current,
            to = ?new_state,
            "VC state transition"
        );

        self.connection_states.insert(guild_id, new_state);
        Ok(())
    }

    /// Get current connection state for a guild
    pub fn get_state(&self, guild_id: u64) -> VcConnectionState {
        self.connection_states
            .get(&guild_id)
            .map(|r| r.clone())
            .unwrap_or(VcConnectionState::Disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_connected() {
        let connected = VcConnectionState::Connected {
            guild_id: 123,
            channel_id: 456,
            connected_at: Instant::now(),
        };
        assert!(connected.is_connected());

        let disconnected = VcConnectionState::Disconnected;
        assert!(!disconnected.is_connected());
    }

    #[test]
    fn state_is_active() {
        let connected = VcConnectionState::Connected {
            guild_id: 123,
            channel_id: 456,
            connected_at: Instant::now(),
        };
        assert!(connected.is_active());

        let reconnecting = VcConnectionState::Reconnecting {
            guild_id: 123,
            channel_id: 456,
            attempt: 1,
            max_attempts: 5,
            last_attempt_at: Instant::now(),
        };
        assert!(reconnecting.is_active());

        let disconnected = VcConnectionState::Disconnected;
        assert!(!disconnected.is_active());
    }

    #[test]
    fn state_guild_id() {
        let connected = VcConnectionState::Connected {
            guild_id: 123,
            channel_id: 456,
            connected_at: Instant::now(),
        };
        assert_eq!(connected.guild_id(), Some(123));

        let disconnected = VcConnectionState::Disconnected;
        assert_eq!(disconnected.guild_id(), None);
    }

    #[test]
    fn gateway_new() {
        let gw = VoiceGateway::new(789);
        assert_eq!(gw.bot_user_id, 789);
    }

    #[test]
    fn valid_state_transitions() {
        let gw = VoiceGateway::new(123);

        // Disconnected → Connecting
        let result = gw.transition(
            1,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id: 1,
                channel_id: 2,
            },
        );
        assert!(result.is_ok());

        // Connecting → Connected
        let result = gw.transition(
            1,
            VcConnectionState::Connected {
                guild_id: 1,
                channel_id: 2,
                connected_at: Instant::now(),
            },
        );
        assert!(result.is_ok());

        // Connected → Disconnected
        let result = gw.transition(1, VcConnectionState::Disconnected);
        assert!(result.is_ok());
    }

    #[test]
    fn invalid_state_transition() {
        let gw = VoiceGateway::new(123);

        // Disconnected → Connected (invalid, must go through Connecting first)
        let result = gw.transition(
            1,
            VcConnectionState::Connected {
                guild_id: 1,
                channel_id: 2,
                connected_at: Instant::now(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn reconnecting_state_transitions() {
        let gw = VoiceGateway::new(123);

        // Set up: Disconnected → Connecting → Connected
        let _ = gw.transition(
            1,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id: 1,
                channel_id: 2,
            },
        );
        let _ = gw.transition(
            1,
            VcConnectionState::Connected {
                guild_id: 1,
                channel_id: 2,
                connected_at: Instant::now(),
            },
        );

        // Connected → Reconnecting
        let result = gw.transition(
            1,
            VcConnectionState::Reconnecting {
                guild_id: 1,
                channel_id: 2,
                attempt: 1,
                max_attempts: 5,
                last_attempt_at: Instant::now(),
            },
        );
        assert!(result.is_ok());

        // Reconnecting → Connected
        let result = gw.transition(
            1,
            VcConnectionState::Connected {
                guild_id: 1,
                channel_id: 2,
                connected_at: Instant::now(),
            },
        );
        assert!(result.is_ok());
    }

    #[test]
    fn reconnecting_give_up() {
        let gw = VoiceGateway::new(123);

        // Set up: Disconnected → Connecting → Connected → Reconnecting
        let _ = gw.transition(
            1,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id: 1,
                channel_id: 2,
            },
        );
        let _ = gw.transition(
            1,
            VcConnectionState::Connected {
                guild_id: 1,
                channel_id: 2,
                connected_at: Instant::now(),
            },
        );
        let _ = gw.transition(
            1,
            VcConnectionState::Reconnecting {
                guild_id: 1,
                channel_id: 2,
                attempt: 5,
                max_attempts: 5,
                last_attempt_at: Instant::now(),
            },
        );

        // Reconnecting → Disconnected (give up after max attempts)
        let result = gw.transition(1, VcConnectionState::Disconnected);
        assert!(result.is_ok());
    }

    #[test]
    fn get_state_default_disconnected() {
        let gw = VoiceGateway::new(123);
        let state = gw.get_state(999);
        assert_eq!(state, VcConnectionState::Disconnected);
    }
}
