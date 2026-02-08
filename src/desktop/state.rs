//! Application state shared between UI and worker

use crate::agent::{SessionInfo, SessionStatus, ToolCall};

/// Message from UI to worker
#[derive(Debug, Clone)]
pub enum UiMessage {
    /// Send a chat message
    Chat(String),
    /// Create a new session
    NewSession,
    /// Resume a session by ID
    ResumeSession(String),
    /// Approve pending tool calls
    ApproveTools(Vec<ToolCall>),
    /// Deny pending tool calls
    DenyTools,
    /// Request session list refresh
    RefreshSessions,
    /// Request status update
    RefreshStatus,
    /// Set model
    SetModel(String),
    /// Compact current session
    Compact,
    /// Search memory
    SearchMemory(String),
    /// Save session to disk
    Save,
    /// Show help text
    ShowHelp,
    /// Show status info
    ShowStatus,
}

/// Message from worker to UI
#[derive(Debug, Clone)]
pub enum WorkerMessage {
    /// Agent is ready
    Ready {
        model: String,
        memory_chunks: usize,
        has_embeddings: bool,
    },
    /// Streaming content chunk
    ContentChunk(String),
    /// Tool call started
    ToolCallStart {
        name: String,
        id: String,
        detail: Option<String>,
    },
    /// Tool call completed
    ToolCallEnd {
        name: String,
        id: String,
        output: String,
    },
    /// Tool calls pending approval
    ToolsPendingApproval(Vec<ToolCall>),
    /// Response complete
    Done,
    /// Error occurred
    Error(String),
    /// Session status update
    Status(SessionStatus),
    /// Session list update
    Sessions(Vec<SessionInfo>),
    /// Session created/resumed
    SessionChanged { id: String, message_count: usize },
    /// System message for display (command output, help text, etc.)
    SystemMessage(String),
}

/// A chat message for display
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub tool_info: Option<ToolInfo>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub detail: Option<String>,
    pub status: ToolStatus,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ToolStatus {
    Running,
    Completed(String), // output preview
    Error(String),
}

/// UI state
#[derive(Default)]
pub struct UiState {
    /// Chat messages to display
    pub messages: Vec<ChatMessage>,
    /// Current input text
    pub input: String,
    /// Whether the agent is processing
    pub is_loading: bool,
    /// Current streaming response (being built)
    pub streaming_content: String,
    /// Active tool calls
    pub active_tools: Vec<ToolInfo>,
    /// Tool calls pending approval
    pub pending_approval: Option<Vec<ToolCall>>,
    /// Error message to display
    pub error: Option<String>,
    /// Available sessions
    pub sessions: Vec<SessionInfo>,
    /// Current session info
    pub current_session: Option<SessionInfo>,
    /// Model name
    pub model: String,
    /// Memory chunk count
    pub memory_chunks: usize,
    /// Whether embeddings are enabled
    pub has_embeddings: bool,
    /// Session status
    pub status: Option<SessionStatus>,
    /// Which panel is active
    pub active_panel: Panel,
    /// Scroll to bottom on next frame
    pub scroll_to_bottom: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Panel {
    #[default]
    Chat,
    Sessions,
    Status,
}

impl UiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a message from the worker
    pub fn handle_worker_message(&mut self, msg: WorkerMessage) {
        match msg {
            WorkerMessage::Ready {
                model,
                memory_chunks,
                has_embeddings,
            } => {
                self.model = model;
                self.memory_chunks = memory_chunks;
                self.has_embeddings = has_embeddings;
                self.is_loading = false;
            }
            WorkerMessage::ContentChunk(content) => {
                self.streaming_content.push_str(&content);
                self.scroll_to_bottom = true;
            }
            WorkerMessage::ToolCallStart {
                name,
                id: _,
                detail,
            } => {
                self.active_tools.push(ToolInfo {
                    name,
                    detail,
                    status: ToolStatus::Running,
                });
            }
            WorkerMessage::ToolCallEnd {
                name,
                output,
                id: _,
            } => {
                // Update tool status
                if let Some(tool) = self.active_tools.iter_mut().find(|t| t.name == name) {
                    let preview = if output.len() > 100 {
                        format!("{}...", &output[..100])
                    } else {
                        output
                    };
                    tool.status = ToolStatus::Completed(preview);
                }
            }
            WorkerMessage::ToolsPendingApproval(calls) => {
                self.pending_approval = Some(calls);
                self.is_loading = false;
            }
            WorkerMessage::Done => {
                // Finalize streaming content as assistant message
                if !self.streaming_content.is_empty() {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: std::mem::take(&mut self.streaming_content),
                        tool_info: None,
                    });
                }
                self.active_tools.clear();
                self.is_loading = false;
                self.scroll_to_bottom = true;
            }
            WorkerMessage::Error(err) => {
                self.error = Some(err);
                self.is_loading = false;
                self.streaming_content.clear();
            }
            WorkerMessage::Status(status) => {
                self.status = Some(status);
            }
            WorkerMessage::Sessions(sessions) => {
                self.sessions = sessions;
            }
            WorkerMessage::SessionChanged { id, message_count } => {
                self.current_session = Some(SessionInfo {
                    id,
                    message_count,
                    created_at: chrono::Utc::now(),
                    file_size: 0,
                });
                // Clear chat on session change
                self.messages.clear();
                self.streaming_content.clear();
            }
            WorkerMessage::SystemMessage(text) => {
                self.messages.push(ChatMessage {
                    role: MessageRole::System,
                    content: text,
                    tool_info: None,
                });
                self.scroll_to_bottom = true;
            }
        }
    }

    /// Add a user message
    pub fn add_user_message(&mut self, content: String) {
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content,
            tool_info: None,
        });
        self.scroll_to_bottom = true;
    }

    /// Clear error
    pub fn clear_error(&mut self) {
        self.error = None;
    }
}
