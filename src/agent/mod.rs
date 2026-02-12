mod providers;
mod sanitize;
mod session;
mod session_store;
mod skills;
mod system_prompt;
mod tools;

pub use providers::{
    ImageAttachment, LLMProvider, LLMResponse, LLMResponseContent, Message, Role, StreamChunk,
    StreamEvent, StreamResult, ToolCall, ToolSchema, Usage,
};
pub use sanitize::{
    EXTERNAL_CONTENT_END, EXTERNAL_CONTENT_START, MEMORY_CONTENT_END, MEMORY_CONTENT_START,
    MemorySource, SanitizeResult, TOOL_OUTPUT_END, TOOL_OUTPUT_START, detect_suspicious_patterns,
    sanitize_tool_output, truncate_with_notice, wrap_external_content, wrap_memory_content,
    wrap_tool_output,
};
pub use session::{
    DEFAULT_AGENT_ID, Session, SessionInfo, SessionMessage, SessionSearchResult, SessionStatus,
    get_last_session_id, get_last_session_id_for_agent, get_sessions_dir_for_agent, get_state_dir,
    list_sessions, list_sessions_for_agent, search_sessions, search_sessions_for_agent,
};
pub use session_store::{SessionEntry, SessionStore};
pub use skills::{Skill, SkillInvocation, get_skills_summary, load_skills, parse_skill_command};
pub use system_prompt::{
    HEARTBEAT_OK_TOKEN, SILENT_REPLY_TOKEN, build_heartbeat_prompt, is_heartbeat_ok,
    is_silent_reply,
};
pub use tools::{Tool, ToolResult, extract_tool_detail};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info};

/// Clean up Claude CLI session files for this workspace.
/// Claude CLI stores sessions in ~/.claude/projects/<workspace-path-with-slashes-as-dashes>/
pub fn clean_claude_cli_sessions(workspace: &std::path::Path) {
    use std::fs;
    let home = match directories::BaseDirs::new() {
        Some(base) => base.home_dir().to_path_buf(),
        None => {
            tracing::warn!("Cannot determine home dir for Claude CLI cleanup");
            return;
        }
    };
    // Convert workspace path to Claude CLI folder name: /Users/kojira/foo -> -Users-kojira-foo
    let workspace_str = workspace.to_string_lossy().replace('/', "-");
    let claude_projects_dir = home.join(".claude").join("projects").join(&workspace_str);
    if !claude_projects_dir.exists() {
        tracing::debug!(
            "No Claude CLI project dir found: {:?}",
            claude_projects_dir
        );
        return;
    }
    // Delete all UUID session directories and .jsonl session files inside the project dir
    match fs::read_dir(&claude_projects_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path
                        .file_name()
                        .map(|n| n.len() == 36)
                        .unwrap_or(false)
                {
                    tracing::info!("Cleaning Claude CLI session dir: {:?}", path);
                    if let Err(e) = fs::remove_dir_all(&path) {
                        tracing::warn!("Failed to remove {:?}: {}", path, e);
                    }
                } else if path.is_file()
                    && path
                        .extension()
                        .map(|ext| ext == "jsonl")
                        .unwrap_or(false)
                {
                    tracing::info!("Cleaning Claude CLI session file: {:?}", path);
                    if let Err(e) = fs::remove_file(&path) {
                        tracing::warn!("Failed to remove {:?}: {}", path, e);
                    }
                }
            }
        }
        Err(e) => tracing::warn!("Failed to read Claude CLI project dir: {}", e),
    }
}

use crate::config::Config;
use crate::memory::{MemoryChunk, MemoryManager};

/// Soft threshold buffer before compaction (tokens)
/// Memory flush runs when within this buffer of the hard limit
const MEMORY_FLUSH_SOFT_THRESHOLD: usize = 4000;

/// Token budget reserved for the per-turn security block (~80 suffix + ~1000 policy + margin).
/// Subtracted from available context to prevent the security block from being dropped
/// during context window management.
const SECURITY_BLOCK_RESERVE: usize = 1200;

/// Generate a URL-safe slug from text (first 3-5 words, lowercased, hyphenated)
fn generate_slug(text: &str) -> String {
    text.split_whitespace()
        .take(4)
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(30)
        .collect()
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub context_window: usize,
    pub reserve_tokens: usize,
}

pub struct Agent {
    config: AgentConfig,
    app_config: Config,
    provider: Box<dyn LLMProvider>,
    session: Session,
    memory: Arc<MemoryManager>,
    tools: Vec<Box<dyn Tool>>,
    /// Cumulative token usage for this session
    cumulative_usage: Usage,
    /// Verified security policy content (None if missing, unsigned, or tampered)
    verified_security_policy: Option<String>,
    /// Last known modification time of SOUL.md for dynamic reload
    soul_last_modified: Option<std::time::SystemTime>,
}

impl Agent {
    pub async fn new(
        config: AgentConfig,
        app_config: &Config,
        memory: MemoryManager,
    ) -> Result<Self> {
        let provider = providers::create_provider(&config.model, app_config)?;

        // Wrap memory in Arc so tools can share it
        let memory = Arc::new(memory);
        let tools = tools::create_default_tools(app_config, Some(Arc::clone(&memory)))?;

        // Load and verify security policy
        let workspace = app_config.workspace_path();
        let state_dir = workspace
            .parent()
            .unwrap_or_else(|| std::path::Path::new("~/.localgpt"));

        let verified_security_policy = if app_config.security.disable_policy {
            debug!("Security policy loading disabled by config");
            None
        } else {
            match crate::security::load_and_verify_policy(&workspace, state_dir) {
                crate::security::PolicyVerification::Valid(content) => {
                    let sha = crate::security::content_sha256(&content);
                    let _ = crate::security::append_audit_entry(
                        state_dir,
                        crate::security::AuditAction::Verified,
                        &sha,
                        "session_start",
                    );
                    info!("Security policy verified and loaded");
                    Some(content)
                }
                crate::security::PolicyVerification::TamperDetected => {
                    let _ = crate::security::append_audit_entry(
                        state_dir,
                        crate::security::AuditAction::TamperDetected,
                        "",
                        "session_start",
                    );
                    if app_config.security.strict_policy {
                        tracing::error!(
                            "LocalGPT.md tamper detected — file was modified after signing"
                        );
                        anyhow::bail!(
                            "Security policy tamper detected. \
                             Re-sign with `localgpt md sign` or remove LocalGPT.md to continue."
                        );
                    }
                    tracing::warn!("LocalGPT.md tamper detected. Using hardcoded security only.");
                    None
                }
                crate::security::PolicyVerification::Unsigned => {
                    let _ = crate::security::append_audit_entry(
                        state_dir,
                        crate::security::AuditAction::Unsigned,
                        "",
                        "session_start",
                    );
                    info!("LocalGPT.md not signed. Run `localgpt md sign` to activate.");
                    None
                }
                crate::security::PolicyVerification::SuspiciousContent(warnings) => {
                    let _ = crate::security::append_audit_entry_with_detail(
                        state_dir,
                        crate::security::AuditAction::SuspiciousContent,
                        "",
                        "session_start",
                        Some(&warnings.join(", ")),
                    );
                    if app_config.security.strict_policy {
                        tracing::error!("LocalGPT.md contains suspicious patterns: {:?}", warnings);
                        anyhow::bail!(
                            "Security policy rejected — suspicious content detected: {}. \
                             Fix LocalGPT.md and re-sign with `localgpt md sign`.",
                            warnings.join(", ")
                        );
                    }
                    tracing::warn!(
                        "LocalGPT.md contains suspicious patterns: {:?}. Skipping.",
                        warnings
                    );
                    None
                }
                crate::security::PolicyVerification::Missing => {
                    let _ = crate::security::append_audit_entry(
                        state_dir,
                        crate::security::AuditAction::Missing,
                        "",
                        "session_start",
                    );
                    debug!("No LocalGPT.md found, using hardcoded security only.");
                    None
                }
                crate::security::PolicyVerification::ManifestCorrupted => {
                    let _ = crate::security::append_audit_entry(
                        state_dir,
                        crate::security::AuditAction::ManifestCorrupted,
                        "",
                        "session_start",
                    );
                    tracing::warn!("Security manifest corrupted. Using hardcoded security only.");
                    None
                }
            }
        };

        Ok(Self {
            config,
            app_config: app_config.clone(),
            provider,
            session: Session::new(),
            memory,
            tools,
            cumulative_usage: Usage::default(),
            verified_security_policy,
            soul_last_modified: None,
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Check if a tool requires user approval before execution
    pub fn requires_approval(&self, tool_name: &str) -> bool {
        self.app_config
            .tools
            .require_approval
            .iter()
            .any(|t| t == tool_name)
    }

    /// Get the list of tools that require approval
    pub fn approval_required_tools(&self) -> &[String] {
        &self.app_config.tools.require_approval
    }

    /// Switch to a different model
    pub fn set_model(&mut self, model: &str) -> Result<()> {
        let provider = providers::create_provider(model, &self.app_config)?;
        self.config.model = model.to_string();
        self.provider = provider;
        info!("Switched to model: {}", model);
        Ok(())
    }

    pub fn memory_chunk_count(&self) -> usize {
        self.memory.chunk_count().unwrap_or(0)
    }

    pub fn has_embeddings(&self) -> bool {
        self.memory.has_embeddings()
    }

    /// Get context window configuration
    pub fn context_window(&self) -> usize {
        self.config.context_window
    }

    /// Get reserve tokens configuration
    pub fn reserve_tokens(&self) -> usize {
        self.config.reserve_tokens
    }

    /// Get current context usage info
    pub fn context_usage(&self) -> (usize, usize, usize) {
        let used = self.session.token_count();
        let available = self.config.context_window;
        let reserve = self.config.reserve_tokens;
        let usable = available.saturating_sub(reserve);
        (used, usable, available)
    }

    /// Export session messages as markdown
    pub fn export_markdown(&self) -> String {
        let mut output = String::new();
        output.push_str("# LocalGPT Session Export\n\n");
        output.push_str(&format!("Model: {}\n", self.config.model));
        output.push_str(&format!("Session ID: {}\n\n", self.session.id()));
        output.push_str("---\n\n");

        for msg in self.session.messages() {
            let role = match msg.role {
                Role::User => "**User**",
                Role::Assistant => "**Assistant**",
                Role::System => "**System**",
                Role::Tool => "**Tool**",
            };
            output.push_str(&format!("{}\n\n{}\n\n---\n\n", role, msg.content));
        }

        output
    }

    /// Get cumulative token usage for this session
    pub fn usage(&self) -> &Usage {
        &self.cumulative_usage
    }

    /// Add usage from an API response to cumulative totals
    fn add_usage(&mut self, usage: Option<Usage>) {
        if let Some(u) = usage {
            self.cumulative_usage.input_tokens += u.input_tokens;
            self.cumulative_usage.output_tokens += u.output_tokens;
        }
    }

    /// Build the message array for an LLM API call, with the security
    /// block injected as a trailing user message on every call.
    ///
    /// This ensures the security suffix always occupies the recency position
    /// (last content before generation), regardless of conversation length.
    /// The security block is synthetic — it is not persisted in session
    /// history and not included in compaction/summarization.
    fn messages_for_api_call(&self) -> Vec<Message> {
        let mut messages = self.session.messages_for_llm();

        let include_suffix = !self.app_config.security.disable_suffix;
        let policy = if self.app_config.security.disable_policy {
            None
        } else {
            self.verified_security_policy.as_deref()
        };

        let security_block = crate::security::build_ending_security_block(policy, include_suffix);

        // Only append if the block has content
        if !security_block.is_empty() {
            messages.push(Message {
                role: Role::User,
                content: security_block,
                tool_calls: None,
                tool_call_id: None,
                images: Vec::new(),
            });
        }

        messages
    }

    pub async fn new_session(&mut self) -> Result<()> {
        self.session = Session::new();

        // Reset provider session state (e.g., clear Claude CLI session ID)
        self.provider.reset_session();

        // Load skills from workspace
        let workspace_skills = skills::load_skills(self.memory.workspace()).unwrap_or_default();
        let skills_prompt = skills::build_skills_prompt(&workspace_skills);
        debug!("Loaded {} skills from workspace", workspace_skills.len());

        // Load SOUL.md first - it defines who the agent is and should come before everything
        let soul_path = self.memory.workspace().join("SOUL.md");
        if let Ok(meta) = soul_path.metadata() {
            if let Ok(modified) = meta.modified() {
                self.soul_last_modified = Some(modified);
            }
        }
        let soul_content = self.read_soul_content();
        let has_soul = !soul_content.is_empty();

        // Build system prompt with identity, safety, workspace info
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
        let system_prompt_params =
            system_prompt::SystemPromptParams::new(self.memory.workspace(), &self.config.model)
                .with_tools(tool_names)
                .with_skills_prompt(skills_prompt);
        let mut system_prompt = system_prompt::build_system_prompt(system_prompt_params);

        // If SOUL.md exists, remove the default identity line and prepend soul content
        if has_soul {
            system_prompt = system_prompt
                .replace(
                    "You are a personal assistant running inside LocalGPT.\n\n",
                    "",
                );
            system_prompt = format!("{}\n\n{}", soul_content, system_prompt);
        }

        // Load memory context (MEMORY.md, daily logs, HEARTBEAT.md, etc. - SOUL.md excluded)
        let memory_context = self.build_memory_context().await?;

        // Combine system prompt with memory context
        let full_context = if memory_context.is_empty() {
            system_prompt
        } else {
            format!(
                "{}\n\n---\n\n# Workspace Context\n\n{}",
                system_prompt, memory_context
            )
        };

        self.session.set_system_context(full_context);

        info!("Created new session: {}", self.session.id());
        Ok(())
    }

    /// Check if SOUL.md has been modified and reload the session if so.
    /// Returns `Ok(true)` if the session was reloaded.
    pub async fn check_and_reload_soul(&mut self) -> Result<bool> {
        let soul_path = self.memory.workspace().join("SOUL.md");
        let current_modified = match soul_path.metadata() {
            Ok(meta) => meta.modified().ok(),
            Err(_) => None,
        };

        let changed = match (current_modified, self.soul_last_modified) {
            (Some(current), Some(previous)) => current != previous,
            (Some(_), None) => true,  // First load
            (None, Some(_)) => true,  // File deleted
            (None, None) => false,
        };

        if changed {
            info!("SOUL.md changed, reloading session");
            clean_claude_cli_sessions(self.memory.workspace());
            self.new_session().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn resume_session(&mut self, session_id: &str) -> Result<()> {
        self.session = Session::load(session_id)?;
        info!("Resumed session: {}", session_id);
        Ok(())
    }

    pub async fn chat(&mut self, message: &str) -> Result<String> {
        self.chat_with_images(message, Vec::new()).await
    }

    pub async fn chat_with_images(
        &mut self,
        message: &str,
        images: Vec<ImageAttachment>,
    ) -> Result<String> {
        // Add user message with images
        self.session.add_message(Message {
            role: Role::User,
            content: message.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images,
        });

        // Check if we should run pre-compaction memory flush (soft threshold)
        if self.should_memory_flush() {
            info!("Running pre-compaction memory flush (soft threshold)");
            self.memory_flush().await?;
        }

        // Check if we need to compact (hard limit)
        if self.should_compact() {
            self.compact_session().await?;
        }

        // Build messages for LLM (with per-turn security block)
        let messages = self.messages_for_api_call();

        // Get available tools
        let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();

        // Invoke LLM
        let response = self
            .provider
            .chat(&messages, Some(tool_schemas.as_slice()))
            .await?;

        // Handle tool calls if any
        let final_response = self.handle_response(response).await?;

        // Add assistant response
        self.session.add_message(Message {
            role: Role::Assistant,
            content: final_response.clone(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });

        Ok(final_response)
    }

    async fn handle_response(&mut self, response: LLMResponse) -> Result<String> {
        // Track usage
        self.add_usage(response.usage);

        match response.content {
            LLMResponseContent::Text(text) => Ok(text),
            LLMResponseContent::ToolCalls(calls) => {
                // Execute tool calls
                let mut results = Vec::new();

                for call in &calls {
                    debug!(
                        "Executing tool: {} with args: {}",
                        call.name, call.arguments
                    );

                    let result = self.execute_tool(call).await;
                    let output = match result {
                        Ok((content, _warnings)) => content,
                        Err(e) => format!("Error: {}", e),
                    };
                    results.push(ToolResult {
                        call_id: call.id.clone(),
                        output,
                    });
                }

                // Add tool call message
                self.session.add_message(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(calls),
                    tool_call_id: None,
                    images: Vec::new(),
                });

                // Add tool results
                for result in &results {
                    self.session.add_message(Message {
                        role: Role::Tool,
                        content: result.output.clone(),
                        tool_calls: None,
                        tool_call_id: Some(result.call_id.clone()),
                        images: Vec::new(),
                    });
                }

                // Continue conversation with tool results (with per-turn security block)
                let messages = self.messages_for_api_call();
                let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();
                let next_response = self
                    .provider
                    .chat(&messages, Some(tool_schemas.as_slice()))
                    .await?;

                // Recursively handle (in case of more tool calls)
                Box::pin(self.handle_response(next_response)).await
            }
        }
    }

    async fn execute_tool(&self, call: &ToolCall) -> Result<(String, Vec<String>)> {
        for tool in &self.tools {
            if tool.name() == call.name {
                let raw_output = tool.execute(&call.arguments).await?;

                // Apply sanitization if configured
                if self.app_config.tools.use_content_delimiters {
                    let max_chars = if self.app_config.tools.tool_output_max_chars > 0 {
                        Some(self.app_config.tools.tool_output_max_chars)
                    } else {
                        None
                    };
                    let result = sanitize::wrap_tool_output(&call.name, &raw_output, max_chars);

                    // Log warnings for suspicious patterns
                    if self.app_config.tools.log_injection_warnings && !result.warnings.is_empty() {
                        tracing::warn!(
                            "Suspicious patterns detected in {} output: {:?}",
                            call.name,
                            result.warnings
                        );
                    }

                    return Ok((result.content, result.warnings));
                }

                return Ok((raw_output, Vec::new()));
            }
        }
        anyhow::bail!("Unknown tool: {}", call.name)
    }

    /// Read SOUL.md content (persona/tone definition).
    /// Extracted so it can be prepended before the system prompt in new_session.
    fn read_soul_content(&self) -> String {
        match self.memory.read_soul_file() {
            Ok(content) if !content.is_empty() => {
                if self.app_config.tools.use_content_delimiters {
                    sanitize::wrap_memory_content(
                        "SOUL.md",
                        &content,
                        sanitize::MemorySource::Soul,
                    )
                } else {
                    content
                }
            }
            _ => String::new(),
        }
    }

    async fn build_memory_context(&self) -> Result<String> {
        let mut context = String::new();
        let use_delimiters = self.app_config.tools.use_content_delimiters;

        // Show welcome message on brand new workspace (first run)
        if self.memory.is_brand_new() {
            context.push_str(FIRST_RUN_WELCOME);
            context.push_str("\n\n---\n\n");
            info!("First run detected - showing welcome message");
        }

        // Load IDENTITY.md first (OpenClaw-compatible: agent identity context)
        if let Ok(identity_content) = self.memory.read_identity_file()
            && !identity_content.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "IDENTITY.md",
                    &identity_content,
                    sanitize::MemorySource::Identity,
                ));
            } else {
                context.push_str("# Identity (IDENTITY.md)\n\n");
                context.push_str(&identity_content);
            }
            context.push_str("\n\n---\n\n");
        }

        // Load USER.md (OpenClaw-compatible: user info)
        if let Ok(user_content) = self.memory.read_user_file()
            && !user_content.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "USER.md",
                    &user_content,
                    sanitize::MemorySource::User,
                ));
            } else {
                context.push_str("# User Info (USER.md)\n\n");
                context.push_str(&user_content);
            }
            context.push_str("\n\n---\n\n");
        }

        // SOUL.md is loaded separately and prepended before system_prompt
        // (see read_soul_content and new_session)

        // Load AGENTS.md (OpenClaw-compatible: list of connected agents)
        if let Ok(agents_content) = self.memory.read_agents_file()
            && !agents_content.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "AGENTS.md",
                    &agents_content,
                    sanitize::MemorySource::Agents,
                ));
            } else {
                context.push_str("# Available Agents (AGENTS.md)\n\n");
                context.push_str(&agents_content);
            }
            context.push_str("\n\n---\n\n");
        }

        // Load TOOLS.md (OpenClaw-compatible: local tool notes)
        if let Ok(tools_content) = self.memory.read_tools_file()
            && !tools_content.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "TOOLS.md",
                    &tools_content,
                    sanitize::MemorySource::Tools,
                ));
            } else {
                context.push_str("# Tool Notes (TOOLS.md)\n\n");
                context.push_str(&tools_content);
            }
            context.push_str("\n\n---\n\n");
        }

        // Load MEMORY.md if it exists
        if let Ok(memory_content) = self.memory.read_memory_file()
            && !memory_content.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "MEMORY.md",
                    &memory_content,
                    sanitize::MemorySource::Memory,
                ));
            } else {
                context.push_str("# Long-term Memory (MEMORY.md)\n\n");
                context.push_str(&memory_content);
            }
            context.push_str("\n\n");
        }

        // Load today's and yesterday's daily logs
        if let Ok(recent_logs) = self.memory.read_recent_daily_logs(2)
            && !recent_logs.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "memory/*.md",
                    &recent_logs,
                    sanitize::MemorySource::DailyLog,
                ));
            } else {
                context.push_str("# Recent Daily Logs\n\n");
                context.push_str(&recent_logs);
            }
            context.push_str("\n\n");
        }

        // Load HEARTBEAT.md if it exists
        if let Ok(heartbeat) = self.memory.read_heartbeat_file()
            && !heartbeat.is_empty()
        {
            if use_delimiters {
                context.push_str(&sanitize::wrap_memory_content(
                    "HEARTBEAT.md",
                    &heartbeat,
                    sanitize::MemorySource::Heartbeat,
                ));
            } else {
                context.push_str("# Pending Tasks (HEARTBEAT.md)\n\n");
                context.push_str(&heartbeat);
            }
            context.push('\n');
        }

        Ok(context)
    }

    fn should_compact(&self) -> bool {
        self.session.token_count()
            > (self.config.context_window - self.config.reserve_tokens - SECURITY_BLOCK_RESERVE)
    }

    /// Check if we should run pre-compaction memory flush (soft threshold)
    fn should_memory_flush(&self) -> bool {
        let hard_limit =
            self.config.context_window - self.config.reserve_tokens - SECURITY_BLOCK_RESERVE;
        let soft_limit = hard_limit.saturating_sub(MEMORY_FLUSH_SOFT_THRESHOLD);

        self.session.token_count() > soft_limit && self.session.should_memory_flush()
    }

    pub async fn compact_session(&mut self) -> Result<(usize, usize)> {
        let before = self.session.token_count();

        // Trigger memory flush before compacting (if not already done)
        if self.session.should_memory_flush() {
            self.memory_flush().await?;
        }

        // Compact the session
        self.session.compact(&*self.provider).await?;

        let after = self.session.token_count();
        info!("Session compacted: {} -> {} tokens", before, after);

        Ok((before, after))
    }

    /// Pre-compaction memory flush - prompts agent to save important info
    /// Runs before compaction to preserve important context to disk
    async fn memory_flush(&mut self) -> Result<()> {
        // Mark as flushed for this compaction cycle (prevents running twice)
        self.session.mark_memory_flushed();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let flush_prompt = format!(
            "Pre-compaction memory flush. Session nearing token limit.\n\
             Store durable memories now (use memory/{}.md; create memory/ if needed).\n\
             - MEMORY.md for persistent facts (user info, preferences, key decisions)\n\
             - memory/{}.md for session notes\n\n\
             If nothing to store, reply: {}",
            today, today, SILENT_REPLY_TOKEN
        );

        // Add flush prompt as user message
        self.session.add_message(Message {
            role: Role::User,
            content: flush_prompt,
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });

        // Get tool schemas so agent can write files
        let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();
        let messages = self.messages_for_api_call();

        let response = self.provider.chat(&messages, Some(&tool_schemas)).await?;

        // Handle response (may include tool calls)
        let final_response = self.handle_response(response).await?;

        // Add response to session
        self.session.add_message(Message {
            role: Role::Assistant,
            content: final_response.clone(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });

        if !is_silent_reply(&final_response) {
            debug!("Memory flush response: {}", final_response);
        }

        Ok(())
    }

    /// Save current session to memory file (called on /new command)
    /// Creates memory/YYYY-MM-DD-slug.md with session transcript
    pub async fn save_session_to_memory(&self) -> Result<Option<PathBuf>> {
        let messages = self.session.user_assistant_messages();

        debug!(
            "save_session_to_memory: {} user/assistant messages found",
            messages.len()
        );

        // Skip if no conversation happened
        if messages.is_empty() {
            debug!("save_session_to_memory: no messages to save, returning None");
            return Ok(None);
        }

        // Get config settings for session memory
        let max_messages = self.app_config.memory.session_max_messages;
        let max_chars = self.app_config.memory.session_max_chars;

        // Limit messages by count (0 = unlimited), take from end like OpenClaw
        let messages: Vec<_> = if max_messages > 0 && messages.len() > max_messages {
            let skip_count = messages.len() - max_messages;
            messages.into_iter().skip(skip_count).collect()
        } else {
            messages
        };

        // Generate slug from first user message
        let slug = messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| generate_slug(&m.content))
            .unwrap_or_else(|| "session".to_string());

        let now = chrono::Local::now();
        let date_str = now.format("%Y-%m-%d").to_string();
        let time_str = now.format("%H:%M:%S").to_string();

        // Build memory file content
        let mut content = format!(
            "# Session: {} {}\n\n\
             - **Session ID**: {}\n\n\
             ## Conversation\n\n",
            date_str,
            time_str,
            self.session.id()
        );

        for msg in &messages {
            let role = match msg.role {
                Role::User => "**User**",
                Role::Assistant => "**Assistant**",
                _ => continue,
            };
            // Only truncate if max_chars > 0 (0 = unlimited, preserves full content)
            let (msg_content, truncated) =
                if max_chars > 0 && msg.content.chars().count() > max_chars {
                    (
                        msg.content.chars().take(max_chars).collect::<String>(),
                        "...",
                    )
                } else {
                    (msg.content.clone(), "")
                };
            content.push_str(&format!("{}: {}{}\n\n", role, msg_content, truncated));
        }

        // Write to memory/YYYY-MM-DD-slug.md
        let memory_dir = self.memory.workspace().join("memory");
        std::fs::create_dir_all(&memory_dir)?;

        let filename = format!("{}-{}.md", date_str, slug);
        let path = memory_dir.join(&filename);

        debug!(
            "save_session_to_memory: writing {} bytes to {}",
            content.len(),
            path.display()
        );
        std::fs::write(&path, content)?;
        info!("Saved session to memory: {}", path.display());

        Ok(Some(path))
    }

    pub fn clear_session(&mut self) {
        self.session = Session::new();
        self.provider.reset_session();
    }

    pub async fn search_memory(&self, query: &str) -> Result<Vec<MemoryChunk>> {
        self.memory.search(query, 10)
    }

    pub async fn reindex_memory(&self) -> Result<(usize, usize, usize)> {
        let stats = self.memory.reindex(true)?;

        // Generate embeddings for new chunks (if embedding provider is configured)
        let (_, embedded) = self.memory.generate_embeddings(50).await?;

        Ok((stats.files_processed, stats.chunks_indexed, embedded))
    }

    pub async fn save_session(&self) -> Result<PathBuf> {
        self.session.save()
    }

    /// Save session for a specific agent ID (used by HTTP server)
    pub async fn save_session_for_agent(&self, agent_id: &str) -> Result<PathBuf> {
        self.session.save_for_agent(agent_id)
    }

    pub fn session_status(&self) -> SessionStatus {
        self.session.status_with_usage(
            self.cumulative_usage.input_tokens,
            self.cumulative_usage.output_tokens,
        )
    }

    /// Stream chat response - returns a stream of chunks
    /// After consuming the stream, call `finish_chat_stream` with the full response
    /// Note: Tool calls during streaming are not yet supported - the model will know
    /// about tools but any tool_use blocks won't be executed automatically.
    pub async fn chat_stream(&mut self, message: &str) -> Result<StreamResult> {
        self.chat_stream_with_images(message, Vec::new()).await
    }

    pub async fn chat_stream_with_images(
        &mut self,
        message: &str,
        images: Vec<ImageAttachment>,
    ) -> Result<StreamResult> {
        // Add user message with images
        self.session.add_message(Message {
            role: Role::User,
            content: message.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images,
        });

        // Check if we should run pre-compaction memory flush (soft threshold)
        if self.should_memory_flush() {
            info!("Running pre-compaction memory flush (soft threshold)");
            self.memory_flush().await?;
        }

        // Check if we need to compact (hard limit)
        if self.should_compact() {
            self.compact_session().await?;
        }

        // Build messages for LLM (with per-turn security block)
        let messages = self.messages_for_api_call();

        // Get tool schemas so the model knows the correct tool call format
        let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();

        // Get stream from provider with tools
        self.provider
            .chat_stream(&messages, Some(&tool_schemas))
            .await
    }

    /// Complete a streaming chat by adding the assistant response to the session
    pub fn finish_chat_stream(&mut self, response: &str) {
        self.session.add_message(Message {
            role: Role::Assistant,
            content: response.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });
    }

    /// Execute tool calls that were accumulated during streaming
    /// Returns (final_response, Vec<(tool_name, warnings)>)
    pub async fn execute_streaming_tool_calls(
        &mut self,
        text_response: &str,
        tool_calls: Vec<ToolCall>,
    ) -> Result<(String, Vec<(String, Vec<String>)>)> {
        // Add assistant message with tool calls
        self.session.add_message(Message {
            role: Role::Assistant,
            content: text_response.to_string(),
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
            images: Vec::new(),
        });

        // Execute each tool and collect results
        let mut results = Vec::new();
        let mut all_warnings: Vec<(String, Vec<String>)> = Vec::new();
        for call in &tool_calls {
            debug!(
                "Executing tool: {} with args: {}",
                call.name, call.arguments
            );

            let result = self.execute_tool(call).await;
            let (output, warnings) = match result {
                Ok((content, warnings)) => (content, warnings),
                Err(e) => (format!("Error: {}", e), Vec::new()),
            };
            if !warnings.is_empty() {
                all_warnings.push((call.name.clone(), warnings));
            }
            results.push(ToolResult {
                call_id: call.id.clone(),
                output,
            });
        }

        // Add tool results to session
        for result in &results {
            self.session.add_message(Message {
                role: Role::Tool,
                content: result.output.clone(),
                tool_calls: None,
                tool_call_id: Some(result.call_id.clone()),
                images: Vec::new(),
            });
        }

        // Get follow-up response from LLM (with per-turn security block)
        let messages = self.messages_for_api_call();
        let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();
        let response = self
            .provider
            .chat(&messages, Some(tool_schemas.as_slice()))
            .await?;

        // Handle the response (may have more tool calls)
        let final_response = self.handle_response(response).await?;

        // Add final response to session
        self.session.add_message(Message {
            role: Role::Assistant,
            content: final_response.clone(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });

        Ok((final_response, all_warnings))
    }

    /// Get a reference to the LLM provider for streaming
    pub fn provider(&self) -> &dyn LLMProvider {
        &*self.provider
    }

    /// Get messages for the LLM (for streaming), with security block.
    pub fn session_messages(&self) -> Vec<Message> {
        self.messages_for_api_call()
    }

    /// Get raw session messages with metadata (for API responses)
    pub fn raw_session_messages(&self) -> &[SessionMessage] {
        self.session.raw_messages()
    }

    /// Add a user message to the session
    pub fn add_user_message(&mut self, content: &str) {
        self.session.add_message(Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });
    }

    /// Add an assistant message to the session
    pub fn add_assistant_message(&mut self, content: &str) {
        self.session.add_message(Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });
    }

    /// Stream chat with tool support
    /// Yields StreamEvent for content chunks and tool executions
    /// Automatically handles tool calls and continues the conversation
    pub async fn chat_stream_with_tools(
        &mut self,
        message: &str,
    ) -> Result<impl futures::Stream<Item = Result<StreamEvent>> + '_> {
        // Add user message
        self.session.add_message(Message {
            role: Role::User,
            content: message.to_string(),
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
        });

        // Check if we should run pre-compaction memory flush (soft threshold)
        if self.should_memory_flush() {
            info!("Running pre-compaction memory flush (soft threshold)");
            self.memory_flush().await?;
        }

        // Check if we need to compact (hard limit)
        if self.should_compact() {
            self.compact_session().await?;
        }

        Ok(self.stream_with_tool_loop())
    }

    fn stream_with_tool_loop(&mut self) -> impl futures::Stream<Item = Result<StreamEvent>> + '_ {
        async_stream::stream! {
            let max_tool_iterations = 10;
            let mut iteration = 0;

            loop {
                iteration += 1;
                if iteration > max_tool_iterations {
                    yield Err(anyhow::anyhow!("Max tool iterations exceeded"));
                    break;
                }

                // Get tool schemas
                let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();

                // Build messages for LLM (with per-turn security block)
                let messages = self.messages_for_api_call();

                // Try streaming first (without tools since most providers don't support tool streaming)
                // Then check for tool calls in the response
                let response = self
                    .provider
                    .chat(&messages, Some(tool_schemas.as_slice()))
                    .await;

                match response {
                    Ok(resp) => {
                        // Track usage
                        self.add_usage(resp.usage);

                        match resp.content {
                            LLMResponseContent::Text(text) => {
                                // No tool calls - yield the text and we're done
                                yield Ok(StreamEvent::Content(text.clone()));
                                yield Ok(StreamEvent::Done);

                                // Add to session
                                self.session.add_message(Message {
                                    role: Role::Assistant,
                                    content: text,
                                    tool_calls: None,
                                    tool_call_id: None,
                                    images: Vec::new(),
                                });
                                break;
                            }
                            LLMResponseContent::ToolCalls(calls) => {
                        // Notify about tool calls
                        for call in &calls {
                            yield Ok(StreamEvent::ToolCallStart {
                                name: call.name.clone(),
                                id: call.id.clone(),
                                arguments: call.arguments.clone(),
                            });

                            // Execute tool
                            let result = self.execute_tool(call).await;
                            let (output, warnings) = match result {
                                Ok((content, warnings)) => (content, warnings),
                                Err(e) => (format!("Error: {}", e), Vec::new()),
                            };

                            yield Ok(StreamEvent::ToolCallEnd {
                                name: call.name.clone(),
                                id: call.id.clone(),
                                output: output.clone(),
                                warnings,
                            });

                            // Add tool result to session
                            self.session.add_message(Message {
                                role: Role::Tool,
                                content: output,
                                tool_calls: None,
                                tool_call_id: Some(call.id.clone()),
                                images: Vec::new(),
                            });
                        }

                        // Add tool call message to session
                        self.session.add_message(Message {
                            role: Role::Assistant,
                            content: String::new(),
                            tool_calls: Some(calls),
                            tool_call_id: None,
                            images: Vec::new(),
                        });

                        // Continue loop to get next response
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                }
            }
        }
    }

    /// Get tool schemas for external use
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|t| t.schema()).collect()
    }

    /// Auto-save session to disk (call after each message)
    pub fn auto_save_session(&self) -> Result<()> {
        self.session.auto_save()
    }
}

/// Welcome message shown on first run (brand new workspace)
const FIRST_RUN_WELCOME: &str = r#"# Welcome to LocalGPT

This is your first session. I've set up a fresh workspace for you.

## Quick Start

1. **Just chat** - I'm ready to help with coding, writing, research, or anything else
2. **Your memory files** are in the workspace:
   - `MEMORY.md` - I'll remember important things here
   - `SOUL.md` - Customize my personality and behavior
   - `HEARTBEAT.md` - Tasks for autonomous mode

## Tell Me About Yourself

What's your name? What kind of projects do you work on? Any preferences for how I should communicate?

I'll save what I learn to MEMORY.md so I remember it next time."#;
