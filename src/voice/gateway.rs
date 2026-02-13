//! Discord Voice Gateway integration via songbird standalone driver.
//!
//! Manages the songbird `Call` lifecycle without serenity,
//! using `ConnectionInfo` built from raw Gateway events forwarded
//! by the existing text-gateway in `src/discord/`.
//!
//! Songbird is configured with `DecodeMode::Decode`, `Channels::Mono`,
//! and `SampleRate::Hz16000` so that VoiceTick events deliver
//! 16 kHz mono i16 PCM directly — no manual Opus decoding or
//! resampling needed.

use anyhow::{Context, Result};
use dashmap::DashMap;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::{Call, ConnectionInfo, CoreEvent, Event};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use super::receiver::{AudioChunk, VoiceReceiveHandler};

// ─── VC connection state machine ────────────────────────────────────

/// VC connection state machine (§22 of the design doc).
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

// ─── Gateway event data ────────────────────────────────────────────

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

// ─── Songbird configuration ────────────────────────────────────────

/// Build a songbird Config that decodes received audio to 16 kHz mono.
fn songbird_receive_config() -> songbird::Config {
    use songbird::driver::{Channels, DecodeMode, SampleRate};
    use std::time::Duration;

    songbird::Config::default()
        .decode_mode(DecodeMode::Decode)
        .decode_channels(Channels::Mono)
        .decode_sample_rate(SampleRate::Hz16000)
        .driver_timeout(Some(Duration::from_secs(30)))
}

// ─── VoiceGateway ──────────────────────────────────────────────────

/// Wraps songbird standalone Calls and manages VC join/leave with a
/// state machine.
pub struct VoiceGateway {
    bot_user_id: u64,
    /// Pending Voice State Update data (waiting for Voice Server Update)
    pending_voice_states: DashMap<u64, VoiceStateData>,
    /// Pending Voice Server Update data (waiting for Voice State Update)
    pending_voice_servers: DashMap<u64, VoiceServerData>,
    /// Connection state per guild
    connection_states: DashMap<u64, VcConnectionState>,
    /// Songbird standalone Call per guild
    calls: DashMap<u64, Arc<Mutex<Call>>>,
    /// Channel to send decoded audio chunks to the dispatcher
    audio_tx: mpsc::UnboundedSender<AudioChunk>,
}

impl VoiceGateway {
    /// Create a new gateway with the bot's user ID and an audio output channel.
    pub fn new(bot_user_id: u64, audio_tx: mpsc::UnboundedSender<AudioChunk>) -> Self {
        Self {
            bot_user_id,
            pending_voice_states: DashMap::new(),
            pending_voice_servers: DashMap::new(),
            connection_states: DashMap::new(),
            calls: DashMap::new(),
            audio_tx,
        }
    }

    /// Join a voice channel (sends Voice State Update via existing gateway).
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

        info!(guild_id, channel_id, "Sent Voice State Update (op=4)");
        Ok(())
    }

    /// Leave a voice channel.
    pub async fn leave(&self, guild_id: u64) -> Result<()> {
        // Disconnect the songbird Call
        if let Some((_, call_arc)) = self.calls.remove(&guild_id) {
            let mut call = call_arc.lock().await;
            let _ = call.leave().await;
            info!(guild_id, "Songbird Call disconnected");
        }

        // Transition to Disconnected
        self.transition(guild_id, VcConnectionState::Disconnected)?;

        // Clean up pending state
        self.pending_voice_states.remove(&guild_id);
        self.pending_voice_servers.remove(&guild_id);

        info!(guild_id, "Left voice channel");
        Ok(())
    }

    /// Handle Voice State Update from Discord Gateway.
    ///
    /// Stores the session_id. If a pending Voice Server Update is already
    /// available for this guild, triggers connection immediately.
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
            self.pending_voice_servers.remove(&data.guild_id);
            // Clean up the Call
            if let Some((_, call_arc)) = self.calls.remove(&data.guild_id) {
                let mut call = call_arc.lock().await;
                let _ = call.leave().await;
            }
            return;
        }

        // Store session_id for later use with Voice Server Update
        let guild_id = data.guild_id;
        self.pending_voice_states.insert(guild_id, data);
        info!(guild_id, "Voice State Update received (session stored)");

        // If Voice Server Update already arrived, connect now
        if self.pending_voice_servers.contains_key(&guild_id) {
            self.try_connect(guild_id).await;
        }
    }

    /// Handle Voice Server Update from Discord Gateway.
    ///
    /// Stores endpoint + token. If a pending Voice State Update (session_id)
    /// is already available for this guild, triggers connection immediately.
    pub async fn handle_voice_server_update(&self, data: VoiceServerData) {
        let guild_id = data.guild_id;
        self.pending_voice_servers.insert(guild_id, data);
        info!(guild_id, "Voice Server Update received (endpoint+token stored)");

        // If Voice State Update already arrived, connect now
        if self.pending_voice_states.contains_key(&guild_id) {
            self.try_connect(guild_id).await;
        } else {
            debug!(
                guild_id,
                "Waiting for Voice State Update before connecting"
            );
        }
    }

    /// Attempt to connect when both Voice State and Voice Server data are available.
    ///
    /// Consumes both pending entries and builds a `ConnectionInfo` for songbird.
    async fn try_connect(&self, guild_id: u64) {
        // Take both pending entries — both must be present
        let state = match self.pending_voice_states.remove(&guild_id) {
            Some((_, s)) => s,
            None => return,
        };
        let server = match self.pending_voice_servers.remove(&guild_id) {
            Some((_, s)) => s,
            None => {
                // Put state back — server hasn't arrived yet
                self.pending_voice_states.insert(guild_id, state);
                return;
            }
        };

        let channel_id = match state.channel_id {
            Some(cid) => cid,
            None => {
                warn!(guild_id, "Voice State has no channel_id");
                return;
            }
        };

        // Build songbird ConnectionInfo
        let guild_nz = match NonZeroU64::new(guild_id) {
            Some(v) => v,
            None => {
                error!("guild_id is zero");
                return;
            }
        };
        let user_nz = match NonZeroU64::new(self.bot_user_id) {
            Some(v) => v,
            None => {
                error!("bot_user_id is zero");
                return;
            }
        };
        let channel_nz = NonZeroU64::new(channel_id);

        // Sanitise endpoint: songbird expects bare host:port, but Discord
        // sometimes sends "wss://host:port" or "host:port/" variants.
        let endpoint = server
            .endpoint
            .trim()
            .strip_prefix("wss://")
            .unwrap_or(&server.endpoint)
            .trim_end_matches('/')
            .to_string();

        debug!(
            guild_id,
            %endpoint,
            session_id = %state.session_id,
            token_len = server.token.len(),
            user_id = self.bot_user_id,
            channel_id,
            "Building ConnectionInfo for songbird (both events received)"
        );

        let connection_info = ConnectionInfo {
            channel_id: channel_nz.map(ChannelId),
            endpoint,
            guild_id: GuildId(guild_nz),
            session_id: state.session_id.clone(),
            token: server.token.clone(),
            user_id: UserId(user_nz),
        };

        // Create or get the standalone Call for this guild
        let call_arc = self
            .calls
            .entry(guild_id)
            .or_insert_with(|| {
                let config = songbird_receive_config();
                let mut call = Call::standalone_from_config(
                    GuildId(guild_nz),
                    UserId(user_nz),
                    config,
                );

                // Register VoiceTick handler for audio reception
                let handler = VoiceReceiveHandler::new(self.audio_tx.clone());
                call.add_global_event(Event::Core(CoreEvent::VoiceTick), handler);

                info!(guild_id, "Created songbird standalone Call");
                Arc::new(Mutex::new(call))
            })
            .clone();

        // Connect using the ConnectionInfo
        let connect_result = {
            let mut call = call_arc.lock().await;
            call.connect(connection_info).await
        };

        match connect_result {
            Ok(()) => {
                // Transition: Connecting → Connected
                if let Err(e) = self.transition(
                    guild_id,
                    VcConnectionState::Connected {
                        guild_id,
                        channel_id,
                        connected_at: Instant::now(),
                    },
                ) {
                    error!(guild_id, error = %e, "State transition failed");
                    return;
                }
                info!(
                    guild_id,
                    channel_id,
                    "Voice connected via songbird standalone driver"
                );
            }
            Err(e) => {
                error!(guild_id, error = %e, "Failed to connect to voice");
                let _ = self.transition(guild_id, VcConnectionState::Disconnected);
                self.calls.remove(&guild_id);
            }
        }
    }

    /// Transition to a new state (validates transition).
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

    /// Get current connection state for a guild.
    pub fn get_state(&self, guild_id: u64) -> VcConnectionState {
        self.connection_states
            .get(&guild_id)
            .map(|r| r.clone())
            .unwrap_or(VcConnectionState::Disconnected)
    }

    /// Play PCM f32 audio (48 kHz mono) through the songbird Call for a guild.
    ///
    /// Converts PCM to WAV in-memory and feeds it to songbird's mixer.
    /// Returns an error if the guild has no active Call.
    pub async fn play_audio(&self, guild_id: u64, pcm: Vec<f32>) -> Result<()> {
        let call_arc = self
            .calls
            .get(&guild_id)
            .ok_or_else(|| anyhow::anyhow!("No active call for guild {}", guild_id))?
            .clone();

        let wav_bytes = crate::voice::audio::pcm_f32_to_wav_bytes(&pcm, 48000)
            .map_err(|e| anyhow::anyhow!("WAV encode failed: {}", e))?;

        let input: songbird::input::Input = wav_bytes.into();
        let mut call = call_arc.lock().await;
        call.play_input(input);

        debug!(guild_id, samples = pcm.len(), "Playing TTS audio via songbird");
        Ok(())
    }

    /// Shut down all active calls.
    pub async fn shutdown(&self) {
        for entry in self.calls.iter() {
            let guild_id = *entry.key();
            let mut call = entry.value().lock().await;
            let _ = call.leave().await;
            info!(guild_id, "Songbird Call shut down");
        }
        self.calls.clear();
        self.connection_states.clear();
        self.pending_voice_states.clear();
        self.pending_voice_servers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_audio_tx() -> mpsc::UnboundedSender<AudioChunk> {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }

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
        let gw = VoiceGateway::new(789, make_audio_tx());
        assert_eq!(gw.bot_user_id, 789);
    }

    #[test]
    fn valid_state_transitions() {
        let gw = VoiceGateway::new(123, make_audio_tx());

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
        let gw = VoiceGateway::new(123, make_audio_tx());

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
        let gw = VoiceGateway::new(123, make_audio_tx());

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
        let gw = VoiceGateway::new(123, make_audio_tx());

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
        let gw = VoiceGateway::new(123, make_audio_tx());
        let state = gw.get_state(999);
        assert_eq!(state, VcConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn handle_voice_state_update_ignores_other_users() {
        let gw = VoiceGateway::new(100, make_audio_tx());
        let data = VoiceStateData {
            guild_id: 1,
            channel_id: Some(2),
            user_id: 999, // different from bot_user_id
            session_id: "sess".to_string(),
        };
        gw.handle_voice_state_update(data).await;
        // Should not store anything
        assert!(gw.pending_voice_states.is_empty());
    }

    #[tokio::test]
    async fn handle_voice_state_update_stores_for_bot() {
        let gw = VoiceGateway::new(100, make_audio_tx());
        let data = VoiceStateData {
            guild_id: 1,
            channel_id: Some(2),
            user_id: 100, // same as bot_user_id
            session_id: "sess".to_string(),
        };
        gw.handle_voice_state_update(data).await;
        assert!(gw.pending_voice_states.contains_key(&1));
    }

    #[tokio::test]
    async fn handle_voice_state_update_disconnect() {
        let gw = VoiceGateway::new(100, make_audio_tx());
        // First, set state to Connecting
        let _ = gw.transition(
            1,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id: 1,
                channel_id: 2,
            },
        );

        let data = VoiceStateData {
            guild_id: 1,
            channel_id: None, // disconnected
            user_id: 100,
            session_id: "sess".to_string(),
        };
        gw.handle_voice_state_update(data).await;
        assert_eq!(gw.get_state(1), VcConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn handle_voice_server_update_without_state() {
        let gw = VoiceGateway::new(100, make_audio_tx());
        let data = VoiceServerData {
            guild_id: 1,
            token: "tok".to_string(),
            endpoint: "wss://example.com".to_string(),
        };
        // Should store server data and wait for Voice State Update
        gw.handle_voice_server_update(data).await;
        // Server data stored, state unchanged (no connect yet)
        assert!(gw.pending_voice_servers.contains_key(&1));
        assert_eq!(gw.get_state(1), VcConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn handle_voice_server_update_first_then_state() {
        let gw = VoiceGateway::new(100, make_audio_tx());

        // Voice Server Update arrives first
        let server = VoiceServerData {
            guild_id: 1,
            token: "tok".to_string(),
            endpoint: "wss://example.com".to_string(),
        };
        gw.handle_voice_server_update(server).await;
        assert!(gw.pending_voice_servers.contains_key(&1));
        // No connect yet
        assert_eq!(gw.get_state(1), VcConnectionState::Disconnected);

        // Set up Connecting state (would happen from join())
        let _ = gw.transition(
            1,
            VcConnectionState::Connecting {
                started_at: Instant::now(),
                guild_id: 1,
                channel_id: 2,
            },
        );

        // Voice State Update arrives second — try_connect will run
        // (will fail to actually connect because there's no real Discord
        //  voice server, but both pending maps should be consumed)
        let state = VoiceStateData {
            guild_id: 1,
            channel_id: Some(2),
            user_id: 100,
            session_id: "sess".to_string(),
        };
        gw.handle_voice_state_update(state).await;

        // Both pending entries consumed by try_connect
        assert!(!gw.pending_voice_states.contains_key(&1));
        assert!(!gw.pending_voice_servers.contains_key(&1));
    }

    #[test]
    fn songbird_receive_config_has_decode_mode() {
        let _config = songbird_receive_config();
        // Config is built without panic
    }
}
