mod providers;
mod session;
mod session_store;
mod skills;
mod system_prompt;
mod tools;

pub use providers::{
    LLMProvider, LLMResponse, Message, Role, StreamChunk, StreamEvent, StreamResult, ToolCall,
    ToolSchema,
};
pub use session::{
    get_last_session_id, get_last_session_id_for_agent, get_sessions_dir_for_agent, get_state_dir,
    list_sessions, list_sessions_for_agent, Session, SessionInfo, SessionStatus, DEFAULT_AGENT_ID,
};
pub use session_store::{SessionEntry, SessionStore};
pub use system_prompt::{
    build_heartbeat_prompt, is_heartbeat_ok, is_silent_reply, HEARTBEAT_OK_TOKEN,
    SILENT_REPLY_TOKEN,
};
pub use tools::{Tool, ToolResult};

use anyhow::Result;
use std::path::PathBuf;
use tracing::{debug, info};

use crate::config::Config;
use crate::memory::{MemoryChunk, MemoryManager};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub context_window: usize,
    pub reserve_tokens: usize,
}

pub struct Agent {
    config: AgentConfig,
    provider: Box<dyn LLMProvider>,
    session: Session,
    memory: MemoryManager,
    tools: Vec<Box<dyn Tool>>,
}

impl Agent {
    pub async fn new(
        config: AgentConfig,
        app_config: &Config,
        memory: MemoryManager,
    ) -> Result<Self> {
        let provider = providers::create_provider(&config.model, app_config)?;
        let tools = tools::create_default_tools(app_config)?;

        Ok(Self {
            config,
            provider,
            session: Session::new(),
            memory,
            tools,
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn memory_chunk_count(&self) -> usize {
        self.memory.chunk_count().unwrap_or(0)
    }

    pub async fn new_session(&mut self) -> Result<()> {
        self.session = Session::new();

        // Load skills from workspace
        let workspace_skills = skills::load_skills(self.memory.workspace()).unwrap_or_default();
        let skills_prompt = skills::build_skills_prompt(&workspace_skills);
        debug!("Loaded {} skills from workspace", workspace_skills.len());

        // Build system prompt with identity, safety, workspace info
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
        let system_prompt_params = system_prompt::SystemPromptParams::new(
            self.memory.workspace(),
            &self.config.model,
        )
        .with_tools(tool_names)
        .with_skills_prompt(skills_prompt);
        let system_prompt = system_prompt::build_system_prompt(system_prompt_params);

        // Load memory context (SOUL.md, MEMORY.md, daily logs, HEARTBEAT.md)
        let memory_context = self.build_memory_context().await?;

        // Combine system prompt with memory context
        let full_context = if memory_context.is_empty() {
            system_prompt
        } else {
            format!("{}\n\n---\n\n# Workspace Context\n\n{}", system_prompt, memory_context)
        };

        self.session.set_system_context(full_context);

        info!("Created new session: {}", self.session.id());
        Ok(())
    }

    pub async fn resume_session(&mut self, session_id: &str) -> Result<()> {
        self.session = Session::load(session_id)?;
        info!("Resumed session: {}", session_id);
        Ok(())
    }

    pub async fn chat(&mut self, message: &str) -> Result<String> {
        // Add user message
        self.session.add_message(Message {
            role: Role::User,
            content: message.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Check if we need to compact
        if self.should_compact() {
            self.compact_session().await?;
        }

        // Build messages for LLM
        let messages = self.session.messages_for_llm();

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
        });

        Ok(final_response)
    }

    async fn handle_response(&mut self, response: LLMResponse) -> Result<String> {
        match response {
            LLMResponse::Text(text) => Ok(text),
            LLMResponse::ToolCalls(calls) => {
                // Execute tool calls
                let mut results = Vec::new();

                for call in &calls {
                    debug!(
                        "Executing tool: {} with args: {}",
                        call.name, call.arguments
                    );

                    let result = self.execute_tool(call).await;
                    results.push(ToolResult {
                        call_id: call.id.clone(),
                        output: result.unwrap_or_else(|e| format!("Error: {}", e)),
                    });
                }

                // Add tool call message
                self.session.add_message(Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(calls),
                    tool_call_id: None,
                });

                // Add tool results
                for result in &results {
                    self.session.add_message(Message {
                        role: Role::Tool,
                        content: result.output.clone(),
                        tool_calls: None,
                        tool_call_id: Some(result.call_id.clone()),
                    });
                }

                // Continue conversation with tool results
                let messages = self.session.messages_for_llm();
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

    async fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        for tool in &self.tools {
            if tool.name() == call.name {
                return tool.execute(&call.arguments).await;
            }
        }
        anyhow::bail!("Unknown tool: {}", call.name)
    }

    async fn build_memory_context(&self) -> Result<String> {
        let mut context = String::new();

        // Load IDENTITY.md first (OpenClaw-compatible: agent identity context)
        if let Ok(identity_content) = self.memory.read_identity_file() {
            if !identity_content.is_empty() {
                context.push_str("# Identity (IDENTITY.md)\n\n");
                context.push_str(&identity_content);
                context.push_str("\n\n---\n\n");
            }
        }

        // Load USER.md (OpenClaw-compatible: user info)
        if let Ok(user_content) = self.memory.read_user_file() {
            if !user_content.is_empty() {
                context.push_str("# User Info (USER.md)\n\n");
                context.push_str(&user_content);
                context.push_str("\n\n---\n\n");
            }
        }

        // Load SOUL.md (persona/tone) - this defines who the agent is
        if let Ok(soul_content) = self.memory.read_soul_file() {
            if !soul_content.is_empty() {
                context.push_str(&soul_content);
                context.push_str("\n\n---\n\n");
            }
        }

        // Load AGENTS.md (OpenClaw-compatible: list of connected agents)
        if let Ok(agents_content) = self.memory.read_agents_file() {
            if !agents_content.is_empty() {
                context.push_str("# Available Agents (AGENTS.md)\n\n");
                context.push_str(&agents_content);
                context.push_str("\n\n---\n\n");
            }
        }

        // Load MEMORY.md if it exists
        if let Ok(memory_content) = self.memory.read_memory_file() {
            if !memory_content.is_empty() {
                context.push_str("# Long-term Memory (MEMORY.md)\n\n");
                context.push_str(&memory_content);
                context.push_str("\n\n");
            }
        }

        // Load today's and yesterday's daily logs
        if let Ok(recent_logs) = self.memory.read_recent_daily_logs(2) {
            if !recent_logs.is_empty() {
                context.push_str("# Recent Daily Logs\n\n");
                context.push_str(&recent_logs);
                context.push_str("\n\n");
            }
        }

        // Load HEARTBEAT.md if it exists
        if let Ok(heartbeat) = self.memory.read_heartbeat_file() {
            if !heartbeat.is_empty() {
                context.push_str("# Pending Tasks (HEARTBEAT.md)\n\n");
                context.push_str(&heartbeat);
                context.push('\n');
            }
        }

        Ok(context)
    }

    fn should_compact(&self) -> bool {
        self.session.token_count() > (self.config.context_window - self.config.reserve_tokens)
    }

    pub async fn compact_session(&mut self) -> Result<(usize, usize)> {
        let before = self.session.token_count();

        // Trigger memory flush before compacting
        self.memory_flush().await?;

        // Compact the session
        self.session.compact(&*self.provider).await?;

        let after = self.session.token_count();
        info!("Session compacted: {} -> {} tokens", before, after);

        Ok((before, after))
    }

    async fn memory_flush(&mut self) -> Result<()> {
        let flush_prompt = format!(
            "Pre-compaction memory flush.\n\
             Store durable memories now (use memory/YYYY-MM-DD.md).\n\
             If nothing to store, reply: {}",
            SILENT_REPLY_TOKEN
        );

        let messages = vec![Message {
            role: Role::System,
            content: flush_prompt,
            tool_calls: None,
            tool_call_id: None,
        }];

        let response = self.provider.chat(&messages, None).await?;

        if let LLMResponse::Text(text) = response {
            if !is_silent_reply(&text) {
                debug!("Memory flush response: {}", text);
            }
        }

        Ok(())
    }

    pub fn clear_session(&mut self) {
        self.session = Session::new();
    }

    pub async fn search_memory(&self, query: &str) -> Result<Vec<MemoryChunk>> {
        self.memory.search(query, 10)
    }

    pub async fn save_session(&self) -> Result<PathBuf> {
        self.session.save()
    }

    pub fn session_status(&self) -> SessionStatus {
        self.session.status()
    }

    /// Stream chat response - returns a stream of chunks
    /// After consuming the stream, call `finish_chat_stream` with the full response
    /// Note: Tool calls during streaming are not yet supported - the model will know
    /// about tools but any tool_use blocks won't be executed automatically.
    pub async fn chat_stream(&mut self, message: &str) -> Result<StreamResult> {
        // Add user message
        self.session.add_message(Message {
            role: Role::User,
            content: message.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Check if we need to compact
        if self.should_compact() {
            self.compact_session().await?;
        }

        // Build messages for LLM
        let messages = self.session.messages_for_llm();

        // Get tool schemas so the model knows the correct tool call format
        let tool_schemas: Vec<ToolSchema> = self.tools.iter().map(|t| t.schema()).collect();

        // Get stream from provider with tools
        self.provider.chat_stream(&messages, Some(&tool_schemas)).await
    }

    /// Complete a streaming chat by adding the assistant response to the session
    pub fn finish_chat_stream(&mut self, response: &str) {
        self.session.add_message(Message {
            role: Role::Assistant,
            content: response.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    /// Get a reference to the LLM provider for streaming
    pub fn provider(&self) -> &dyn LLMProvider {
        &*self.provider
    }

    /// Get messages for the LLM (for streaming)
    pub fn session_messages(&self) -> Vec<Message> {
        self.session.messages_for_llm()
    }

    /// Add a user message to the session
    pub fn add_user_message(&mut self, content: &str) {
        self.session.add_message(Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    /// Add an assistant message to the session
    pub fn add_assistant_message(&mut self, content: &str) {
        self.session.add_message(Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
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
        });

        // Check if we need to compact
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

                // Build messages for LLM
                let messages = self.session.messages_for_llm();

                // Try streaming first (without tools since most providers don't support tool streaming)
                // Then check for tool calls in the response
                let response = self
                    .provider
                    .chat(&messages, Some(tool_schemas.as_slice()))
                    .await;

                match response {
                    Ok(LLMResponse::Text(text)) => {
                        // No tool calls - yield the text and we're done
                        yield Ok(StreamEvent::Content(text.clone()));
                        yield Ok(StreamEvent::Done);

                        // Add to session
                        self.session.add_message(Message {
                            role: Role::Assistant,
                            content: text,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        break;
                    }
                    Ok(LLMResponse::ToolCalls(calls)) => {
                        // Notify about tool calls
                        for call in &calls {
                            yield Ok(StreamEvent::ToolCallStart {
                                name: call.name.clone(),
                                id: call.id.clone(),
                            });

                            // Execute tool
                            let result = self.execute_tool(call).await;
                            let output = result.unwrap_or_else(|e| format!("Error: {}", e));

                            yield Ok(StreamEvent::ToolCallEnd {
                                name: call.name.clone(),
                                id: call.id.clone(),
                                output: output.clone(),
                            });

                            // Add tool result to session
                            self.session.add_message(Message {
                                role: Role::Tool,
                                content: output,
                                tool_calls: None,
                                tool_call_id: Some(call.id.clone()),
                            });
                        }

                        // Add tool call message to session
                        self.session.add_message(Message {
                            role: Role::Assistant,
                            content: String::new(),
                            tool_calls: Some(calls),
                            tool_call_id: None,
                        });

                        // Continue loop to get next response
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
