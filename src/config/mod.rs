mod migrate;
mod schema;

pub use migrate::{has_openclaw_workspace, openclaw_config_path, try_migrate_openclaw_config};
pub use schema::*;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub providers: ProvidersConfig,

    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    #[serde(default)]
    pub memory: MemoryConfig,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    #[serde(default)]
    pub tools: ToolsConfig,

    #[serde(default)]
pub security: SecurityConfig,

    #[serde(default)]
    pub sandbox: SandboxConfig,

    #[serde(default)]
    pub telegram: Option<TelegramConfig>,

    #[serde(default)]
    pub channels: ChannelsConfig,

    #[serde(default)]
    pub tags: HashMap<String, TagGroup>,

    #[serde(default)]
    pub voice: Option<VoiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub discord: VoiceDiscordConfig,

    #[serde(default)]
    pub pipeline: VoicePipelineConfig,

    #[serde(default)]
    pub stt: VoiceSttConfig,

    #[serde(default)]
    pub tts: VoiceTtsConfig,

    #[serde(default)]
    pub agent: VoiceAgentConfig,

    #[serde(default)]
    pub audio: VoiceAudioConfig,

    #[serde(default)]
    pub transcript: VoiceTranscriptConfig,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discord: VoiceDiscordConfig::default(),
            pipeline: VoicePipelineConfig::default(),
            stt: VoiceSttConfig::default(),
            tts: VoiceTtsConfig::default(),
            agent: VoiceAgentConfig::default(),
            audio: VoiceAudioConfig::default(),
            transcript: VoiceTranscriptConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceDiscordConfig {
    #[serde(default)]
    pub auto_join: Vec<VoiceAutoJoinEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceAutoJoinEntry {
    pub guild_id: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePipelineConfig {
    #[serde(default = "default_true")]
    pub interrupt_enabled: bool,

    /// Idle timeout in seconds (0 = disabled)
    #[serde(default = "default_voice_idle_timeout")]
    pub idle_timeout_sec: u64,

    /// Multi-user context window duration in milliseconds.
    /// STT finals within this window are batched into a single LLM request.
    #[serde(default = "default_voice_context_window_ms")]
    pub context_window_ms: u64,

    /// Automatically enable context window when multiple users are present.
    #[serde(default = "default_true")]
    pub context_window_auto: bool,
}

impl Default for VoicePipelineConfig {
    fn default() -> Self {
        Self {
            interrupt_enabled: true,
            idle_timeout_sec: default_voice_idle_timeout(),
            context_window_ms: default_voice_context_window_ms(),
            context_window_auto: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceSttConfig {
    #[serde(default = "default_voice_stt_provider")]
    pub provider: String,

    /// Maximum number of concurrent STT sessions (1–8, default: 4).
    #[serde(default = "default_voice_stt_max_concurrent")]
    pub max_concurrent_sessions: usize,

    #[serde(default)]
    pub ws: VoiceSttWsConfig,
}

impl Default for VoiceSttConfig {
    fn default() -> Self {
        Self {
            provider: default_voice_stt_provider(),
            max_concurrent_sessions: default_voice_stt_max_concurrent(),
            ws: VoiceSttWsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceSttWsConfig {
    #[serde(default = "default_voice_stt_ws_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_voice_stt_reconnect_interval")]
    pub reconnect_interval_ms: u64,

    #[serde(default = "default_voice_stt_max_reconnect")]
    pub max_reconnect_attempts: u32,
}

impl Default for VoiceSttWsConfig {
    fn default() -> Self {
        Self {
            endpoint: default_voice_stt_ws_endpoint(),
            reconnect_interval_ms: default_voice_stt_reconnect_interval(),
            max_reconnect_attempts: default_voice_stt_max_reconnect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTtsConfig {
    #[serde(default = "default_voice_tts_provider")]
    pub provider: String,

    #[serde(default)]
    pub aivis_speech: VoiceTtsAivisSpeechConfig,
}

impl Default for VoiceTtsConfig {
    fn default() -> Self {
        Self {
            provider: default_voice_tts_provider(),
            aivis_speech: VoiceTtsAivisSpeechConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTtsAivisSpeechConfig {
    #[serde(default = "default_voice_tts_aivis_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_voice_tts_aivis_model")]
    pub model: String,

    #[serde(default = "default_voice_tts_speed_scale")]
    pub speed_scale: f64,

    #[serde(default = "default_voice_tts_volume_scale")]
    pub volume_scale: f64,
}

impl Default for VoiceTtsAivisSpeechConfig {
    fn default() -> Self {
        Self {
            endpoint: default_voice_tts_aivis_endpoint(),
            model: default_voice_tts_aivis_model(),
            speed_scale: default_voice_tts_speed_scale(),
            volume_scale: default_voice_tts_volume_scale(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoiceAgentConfig {
    /// Override system prompt for voice sessions
    #[serde(default)]
    pub system_prompt_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceAudioConfig {
    #[serde(default = "default_voice_input_sample_rate")]
    pub input_sample_rate: u32,

    #[serde(default = "default_voice_stt_sample_rate")]
    pub stt_sample_rate: u32,

    #[serde(default = "default_voice_playback_prebuffer")]
    pub playback_prebuffer_ms: u32,
}

impl Default for VoiceAudioConfig {
    fn default() -> Self {
        Self {
            input_sample_rate: default_voice_input_sample_rate(),
            stt_sample_rate: default_voice_stt_sample_rate(),
            playback_prebuffer_ms: default_voice_playback_prebuffer(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTranscriptConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Text channel ID for posting transcripts
    #[serde(default)]
    pub channel_id: Option<String>,
}

impl Default for VoiceTranscriptConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_model")]
    pub default_model: String,

    #[serde(default = "default_context_window")]
    pub context_window: usize,

    #[serde(default = "default_reserve_tokens")]
    pub reserve_tokens: usize,

    /// Maximum tokens for LLM response
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    /// Bash command timeout in milliseconds
    #[serde(default = "default_bash_timeout")]
    pub bash_timeout_ms: u64,

    /// Maximum bytes to return from web_fetch
    #[serde(default = "default_web_fetch_max_bytes")]
    pub web_fetch_max_bytes: usize,

    /// Tools that require user approval before execution
    /// e.g., ["bash", "write_file", "edit_file"]
    #[serde(default)]
    pub require_approval: Vec<String>,

    /// Maximum characters for tool output (0 = unlimited)
    #[serde(default = "default_tool_output_max_chars")]
    pub tool_output_max_chars: usize,

    /// Log warnings for suspicious injection patterns detected in tool outputs
    #[serde(default = "default_true")]
    pub log_injection_warnings: bool,

    /// Wrap tool outputs and memory content with XML-style delimiters
    #[serde(default = "default_true")]
    pub use_content_delimiters: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Abort agent startup on tamper or suspicious content (default: false)
    ///
    /// When true, `TamperDetected` and `SuspiciousContent` are fatal errors
    /// that prevent the agent from starting. When false (default), the agent
    /// warns and falls back to hardcoded-only security.
    #[serde(default)]
    pub strict_policy: bool,

    /// Skip loading and injecting the LocalGPT.md security policy (default: false)
    #[serde(default)]
    pub disable_policy: bool,

    /// Skip injecting the hardcoded security suffix (default: false)
    #[serde(default)]
    pub disable_suffix: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Enable shell command sandboxing (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Sandbox level: "auto" | "full" | "standard" | "minimal" | "none"
    #[serde(default = "default_sandbox_level")]
    pub level: String,

    /// Command timeout in seconds (default: 120)
    #[serde(default = "default_sandbox_timeout")]
    pub timeout_secs: u64,

    /// Maximum output bytes (default: 1MB)
    #[serde(default = "default_sandbox_max_output")]
    pub max_output_bytes: u64,

    /// Maximum file size in bytes (RLIMIT_FSIZE, default: 50MB)
    #[serde(default = "default_sandbox_max_file_size")]
    pub max_file_size_bytes: u64,

    /// Maximum child processes (RLIMIT_NPROC, default: 64)
    #[serde(default = "default_sandbox_max_processes")]
    pub max_processes: u32,

    /// Additional path allowances
    #[serde(default)]
    pub allow_paths: AllowPathsConfig,

    /// Network policy
    #[serde(default)]
    pub network: SandboxNetworkConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowPathsConfig {
    /// Additional read-only paths
    #[serde(default)]
    pub read: Vec<String>,

    /// Additional writable paths
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxNetworkConfig {
    /// Network policy: "deny" | "proxy"
    #[serde(default = "default_sandbox_network_policy")]
    pub policy: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            level: default_sandbox_level(),
            timeout_secs: default_sandbox_timeout(),
            max_output_bytes: default_sandbox_max_output(),
            max_file_size_bytes: default_sandbox_max_file_size(),
            max_processes: default_sandbox_max_processes(),
            allow_paths: AllowPathsConfig::default(),
            network: SandboxNetworkConfig::default(),
        }
    }
}

impl Default for SandboxNetworkConfig {
    fn default() -> Self {
        Self {
            policy: default_sandbox_network_policy(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub openai: Option<OpenAIConfig>,

    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,

    #[serde(default)]
    pub ollama: Option<OllamaConfig>,

    #[serde(default)]
    pub claude_cli: Option<ClaudeCliConfig>,

    #[serde(default)]
    pub glm: Option<GlmConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    pub api_key: String,

    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key: String,

    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    #[serde(default = "default_ollama_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_ollama_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCliConfig {
    #[serde(default = "default_claude_cli_command")]
    pub command: String,

    #[serde(default = "default_claude_cli_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlmConfig {
    pub api_key: String,

    #[serde(default = "default_glm_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_interval")]
    pub interval: String,

    #[serde(default)]
    pub active_hours: Option<ActiveHours>,

    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveHours {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_workspace")]
    pub workspace: String,

    /// Embedding provider: "local" (fastembed, default), "openai", or "none"
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,

    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,

    /// Cache directory for local embedding models (optional)
    /// Default: ~/.cache/localgpt/models
    /// Can also be set via FASTEMBED_CACHE_DIR environment variable
    #[serde(default = "default_embedding_cache_dir")]
    pub embedding_cache_dir: String,

    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,

    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap: usize,

    /// Additional paths to index (relative to workspace or absolute)
    /// Each path uses a glob pattern for file matching
    #[serde(default = "default_index_paths")]
    pub paths: Vec<MemoryIndexPath>,

    /// Maximum messages to save in session memory files (0 = unlimited)
    /// Similar to OpenClaw's hooks.session-memory.messages (default: 15)
    #[serde(default = "default_session_max_messages")]
    pub session_max_messages: usize,

    /// Maximum characters per message in session memory (0 = unlimited)
    /// Set to 0 to preserve full message content like OpenClaw
    #[serde(default)]
    pub session_max_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryIndexPath {
    pub path: String,
    #[serde(default = "default_pattern")]
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,

    #[serde(default = "default_log_file")]
    pub file: String,

    /// Days to keep log files (0 = keep forever, no auto-deletion)
    #[serde(default)]
    pub retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,

    pub api_token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub discord: Option<DiscordChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordChannelConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Bot token (use ${DISCORD_BOT_TOKEN} for env var expansion)
    pub token: String,

    /// Whether to process messages from other bots
    #[serde(default)]
    pub allow_bots: bool,

    /// Guild (server) allow-list with per-guild settings
    #[serde(default)]
    pub guilds: Vec<DiscordGuildConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordGuildConfig {
    pub guild_id: String,

    /// Channel IDs the bot listens in (empty = all channels)
    #[serde(default)]
    pub channels: Vec<String>,

    /// Whether the bot must be @mentioned to respond
    #[serde(default)]
    pub require_mention: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagGroup {
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub config_swap: Option<String>,
    #[serde(default)]
    pub patterns: HashMap<String, String>,
}

// Default value functions
fn default_model() -> String {
    // Default to Claude CLI (uses existing Claude Code auth, no API key needed)
    "claude-cli/opus".to_string()
}
fn default_context_window() -> usize {
    128000
}
fn default_reserve_tokens() -> usize {
    8000
}
fn default_max_tokens() -> usize {
    4096
}
fn default_bash_timeout() -> u64 {
    30000 // 30 seconds
}
fn default_web_fetch_max_bytes() -> usize {
    10000
}
fn default_tool_output_max_chars() -> usize {
    50000 // 50k characters max for tool output by default
}
fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".to_string()
}
fn default_ollama_endpoint() -> String {
    "http://localhost:11434".to_string()
}
fn default_ollama_model() -> String {
    "llama3".to_string()
}
fn default_claude_cli_command() -> String {
    "claude".to_string()
}
fn default_claude_cli_model() -> String {
    "opus".to_string()
}
fn default_glm_base_url() -> String {
    "https://api.z.ai/api/coding/paas/v4".to_string()
}
fn default_true() -> bool {
    true
}
fn default_interval() -> String {
    "30m".to_string()
}
fn default_workspace() -> String {
    "~/.localgpt/workspace".to_string()
}
fn default_embedding_provider() -> String {
    "local".to_string() // Local embeddings via fastembed (no API key needed)
}
fn default_embedding_model() -> String {
    "all-MiniLM-L6-v2".to_string() // Local model via fastembed (no API key needed)
}
fn default_embedding_cache_dir() -> String {
    "~/.cache/localgpt/models".to_string()
}
fn default_chunk_size() -> usize {
    400
}
fn default_chunk_overlap() -> usize {
    80
}
fn default_index_paths() -> Vec<MemoryIndexPath> {
    vec![MemoryIndexPath {
        path: "knowledge".to_string(),
        pattern: "**/*.md".to_string(),
    }]
}
fn default_pattern() -> String {
    "**/*.md".to_string()
}
fn default_session_max_messages() -> usize {
    15 // Match OpenClaw's default
}
fn default_port() -> u16 {
    31327
}
fn default_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_file() -> String {
    "~/.localgpt/logs/agent.log".to_string()
}
fn default_sandbox_level() -> String {
    "auto".to_string()
}
fn default_sandbox_timeout() -> u64 {
    120
}
fn default_sandbox_max_output() -> u64 {
    1_048_576 // 1MB
}
fn default_sandbox_max_file_size() -> u64 {
    52_428_800 // 50MB
}
fn default_sandbox_max_processes() -> u32 {
    64
}
fn default_sandbox_network_policy() -> String {
    "deny".to_string()
}

// Voice config defaults
fn default_voice_idle_timeout() -> u64 {
    300 // 5 minutes
}
fn default_voice_stt_provider() -> String {
    "ws".to_string()
}
fn default_voice_stt_ws_endpoint() -> String {
    "ws://127.0.0.1:8766/ws".to_string()
}
fn default_voice_stt_reconnect_interval() -> u64 {
    1000
}
fn default_voice_stt_max_reconnect() -> u32 {
    10
}
fn default_voice_tts_provider() -> String {
    "aivis-speech".to_string()
}
fn default_voice_tts_aivis_endpoint() -> String {
    "http://127.0.0.1:8001".to_string()
}
fn default_voice_tts_aivis_model() -> String {
    "donchan".to_string()
}
fn default_voice_tts_speed_scale() -> f64 {
    1.0
}
fn default_voice_tts_volume_scale() -> f64 {
    1.0
}
fn default_voice_input_sample_rate() -> u32 {
    48000
}
fn default_voice_stt_sample_rate() -> u32 {
    16000
}
fn default_voice_playback_prebuffer() -> u32 {
    100
}
fn default_voice_stt_max_concurrent() -> usize {
    4
}
fn default_voice_context_window_ms() -> u64 {
    2000
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            context_window: default_context_window(),
            reserve_tokens: default_reserve_tokens(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash_timeout_ms: default_bash_timeout(),
            web_fetch_max_bytes: default_web_fetch_max_bytes(),
            require_approval: Vec::new(),
            tool_output_max_chars: default_tool_output_max_chars(),
            log_injection_warnings: default_true(),
            use_content_delimiters: default_true(),
        }
    }
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            interval: default_interval(),
            active_hours: None,
            timezone: None,
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_cache_dir: default_embedding_cache_dir(),
            chunk_size: default_chunk_size(),
            chunk_overlap: default_chunk_overlap(),
            paths: default_index_paths(),
            session_max_messages: default_session_max_messages(),
            session_max_chars: 0, // 0 = unlimited (preserve full content like OpenClaw)
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            port: default_port(),
            bind: default_bind(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: default_log_file(),
            retention_days: 0, // 0 = keep forever
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;

        if !path.exists() {
            // Try to migrate from OpenClaw config
            if let Some(migrated) = try_migrate_openclaw_config() {
                // Save migrated config to disk
                migrated.save()?;
                return Ok(migrated);
            }
            // Create default config file on first run
            let config = Config::default();
            config.save_with_template()?;
            return Ok(config);
        }

        let content = fs::read_to_string(&path)?;
        let mut config: Config = toml::from_str(&content)?;

        // Expand environment variables in API keys
        config.expand_env_vars();

        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;

        Ok(())
    }

    /// Save config with a helpful template (for first-time setup)
    pub fn save_with_template(&self) -> Result<()> {
        let path = Self::config_path()?;

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
        eprintln!("Created default config at {}", path.display());

        Ok(())
    }

    pub fn config_path() -> Result<PathBuf> {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

        Ok(base.home_dir().join(".localgpt").join("config.toml"))
    }

    fn expand_env_vars(&mut self) {
        if let Some(ref mut openai) = self.providers.openai {
            openai.api_key = expand_env(&openai.api_key);
        }
        if let Some(ref mut anthropic) = self.providers.anthropic {
            anthropic.api_key = expand_env(&anthropic.api_key);
        }
if let Some(ref mut telegram) = self.telegram {
            telegram.api_token = expand_env(&telegram.api_token);
        }
        if let Some(ref mut discord) = self.channels.discord {
            discord.token = expand_env(&discord.token);
        }
    }

    pub fn get_value(&self, key: &str) -> Result<String> {
        let parts: Vec<&str> = key.split('.').collect();

        match parts.as_slice() {
            ["agent", "default_model"] => Ok(self.agent.default_model.clone()),
            ["agent", "context_window"] => Ok(self.agent.context_window.to_string()),
            ["agent", "reserve_tokens"] => Ok(self.agent.reserve_tokens.to_string()),
            ["heartbeat", "enabled"] => Ok(self.heartbeat.enabled.to_string()),
            ["heartbeat", "interval"] => Ok(self.heartbeat.interval.clone()),
            ["server", "enabled"] => Ok(self.server.enabled.to_string()),
            ["server", "port"] => Ok(self.server.port.to_string()),
            ["server", "bind"] => Ok(self.server.bind.clone()),
            ["memory", "workspace"] => Ok(self.memory.workspace.clone()),
            ["logging", "level"] => Ok(self.logging.level.clone()),
            _ => anyhow::bail!("Unknown config key: {}", key),
        }
    }

    pub fn set_value(&mut self, key: &str, value: &str) -> Result<()> {
        let parts: Vec<&str> = key.split('.').collect();

        match parts.as_slice() {
            ["agent", "default_model"] => self.agent.default_model = value.to_string(),
            ["agent", "context_window"] => self.agent.context_window = value.parse()?,
            ["agent", "reserve_tokens"] => self.agent.reserve_tokens = value.parse()?,
            ["heartbeat", "enabled"] => self.heartbeat.enabled = value.parse()?,
            ["heartbeat", "interval"] => self.heartbeat.interval = value.to_string(),
            ["server", "enabled"] => self.server.enabled = value.parse()?,
            ["server", "port"] => self.server.port = value.parse()?,
            ["server", "bind"] => self.server.bind = value.to_string(),
            ["memory", "workspace"] => self.memory.workspace = value.to_string(),
            ["logging", "level"] => self.logging.level = value.to_string(),
            _ => anyhow::bail!("Unknown config key: {}", key),
        }

        Ok(())
    }

    /// Get workspace path, expanded
    ///
    /// Resolution order (like OpenClaw):
    /// 1. LOCALGPT_WORKSPACE env var (absolute path override)
    /// 2. LOCALGPT_PROFILE env var (creates ~/.localgpt/workspace-{profile})
    /// 3. memory.workspace from config file
    /// 4. Default: ~/.localgpt/workspace
    pub fn workspace_path(&self) -> PathBuf {
        // Check for direct workspace override
        if let Ok(workspace) = std::env::var("LOCALGPT_WORKSPACE") {
            let trimmed = workspace.trim();
            if !trimmed.is_empty() {
                let expanded = shellexpand::tilde(trimmed);
                return PathBuf::from(expanded.to_string());
            }
        }

        // Check for profile-based workspace (like OpenClaw's OPENCLAW_PROFILE)
        if let Ok(profile) = std::env::var("LOCALGPT_PROFILE") {
            let trimmed = profile.trim().to_lowercase();
            if !trimmed.is_empty() && trimmed != "default" {
                let base = directories::BaseDirs::new()
                    .map(|b| b.home_dir().to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("~"));
                return base
                    .join(".localgpt")
                    .join(format!("workspace-{}", trimmed));
            }
        }

        // Use config value
        let expanded = shellexpand::tilde(&self.memory.workspace);
        PathBuf::from(expanded.to_string())
    }
}

fn expand_env(s: &str) -> String {
    if let Some(var_name) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var_name).unwrap_or_else(|_| s.to_string())
    } else if let Some(var_name) = s.strip_prefix('$') {
        std::env::var(var_name).unwrap_or_else(|_| s.to_string())
    } else {
        s.to_string()
    }
}

/// Default config template with helpful comments (used for first-time setup)
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# LocalGPT Configuration
# Auto-created on first run. Edit as needed.

[agent]
# Default model: claude-cli/opus, anthropic/claude-sonnet-4-5, openai/gpt-4o, etc.
default_model = "claude-cli/opus"
context_window = 128000
reserve_tokens = 8000

# Anthropic API (for anthropic/* models)
# [providers.anthropic]
# api_key = "${ANTHROPIC_API_KEY}"

# OpenAI API (for openai/* models)
# [providers.openai]
# api_key = "${OPENAI_API_KEY}"

# Claude CLI (for claude-cli/* models, requires claude CLI installed)
[providers.claude_cli]
command = "claude"

[heartbeat]
enabled = true
interval = "30m"

# Only run during these hours (optional)
# [heartbeat.active_hours]
# start = "09:00"
# end = "22:00"

[memory]
# Workspace directory for memory files (MEMORY.md, HEARTBEAT.md, etc.)
# Can also be set via environment variables:
#   LOCALGPT_WORKSPACE=/path/to/workspace  - absolute path override
#   LOCALGPT_PROFILE=work                  - uses ~/.localgpt/workspace-work
workspace = "~/.localgpt/workspace"

# Session memory settings (for /new command)
# session_max_messages = 15    # Max messages to save (0 = unlimited)
# session_max_chars = 0        # Max chars per message (0 = unlimited, preserves full content)

[server]
enabled = true
port = 31327
bind = "127.0.0.1"

[logging]
level = "info"

# Shell sandbox (kernel-enforced isolation for LLM-generated commands)
# [sandbox]
# enabled = true                        # default: true
# level = "auto"                        # auto | full | standard | minimal | none
# timeout_secs = 120                    # default: 120
# max_output_bytes = 1048576            # default: 1MB
#
# [sandbox.allow_paths]
# read = ["/data/datasets"]             # additional read-only paths
# write = ["/tmp/builds"]               # additional writable paths
#
# [sandbox.network]
# policy = "deny"                       # deny | proxy

# Telegram bot (optional)
# [telegram]
# enabled = true
# api_token = "${TELEGRAM_BOT_TOKEN}"
"#;
