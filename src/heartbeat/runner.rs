//! Heartbeat runner for continuous autonomous operation

use anyhow::Result;
use chrono::{Local, NaiveTime};
use std::fs;
use std::time::Instant;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::events::{HeartbeatEvent, HeartbeatStatus, emit_heartbeat_event, now_ms};
use crate::agent::{
    Agent, AgentConfig, HEARTBEAT_OK_TOKEN, SessionStore, build_heartbeat_prompt, is_heartbeat_ok,
};
use crate::concurrency::{TurnGate, WorkspaceLock};
use crate::config::{Config, SharedConfig, parse_duration, parse_time};
use crate::memory::MemoryManager;

pub struct HeartbeatRunner {
    shared_config: SharedConfig,
    agent_id: String,
    /// Cached MemoryManager to avoid reinitializing embedding provider on every heartbeat
    memory: MemoryManager,
    /// In-process turn gate (shared with HTTP server when running in daemon)
    turn_gate: Option<TurnGate>,
    /// Cross-process workspace lock
    workspace_lock: WorkspaceLock,
}

impl HeartbeatRunner {
    /// Create a new HeartbeatRunner with the default agent ID ("main")
    pub async fn new(config: &Config) -> Result<Self> {
        Self::new_with_agent(config, "main").await
    }

    /// Create a new HeartbeatRunner for a specific agent ID (standalone, no daemon)
    pub async fn new_with_agent(config: &Config, agent_id: &str) -> Result<Self> {
        let shared =
            std::sync::Arc::new(tokio::sync::RwLock::new(config.clone()));
        Self::new_with_gate(shared, agent_id, None).await
    }

    /// Create a new HeartbeatRunner with an optional in-process TurnGate.
    ///
    /// When running inside the daemon alongside the HTTP server, pass a
    /// shared `TurnGate` and `SharedConfig` so heartbeat uses the latest config each cycle.
    pub async fn new_with_gate(
        shared_config: SharedConfig,
        agent_id: &str,
        turn_gate: Option<TurnGate>,
    ) -> Result<Self> {
        let config = shared_config.read().await.clone();
        let memory =
            MemoryManager::new_with_full_config(&config.memory, Some(&config), agent_id)?;
        let workspace_lock = WorkspaceLock::new()?;

        Ok(Self {
            shared_config,
            agent_id: agent_id.to_string(),
            memory,
            turn_gate,
            workspace_lock,
        })
    }

    /// Run the heartbeat loop continuously
    pub async fn run(&self) -> Result<()> {
        let config = self.shared_config.read().await.clone();
        let interval = parse_duration(&config.heartbeat.interval)
            .map_err(|e| anyhow::anyhow!("Invalid heartbeat interval: {}", e))?;
        info!("Starting heartbeat runner with interval: {:?}", interval);

        loop {
            // Sleep until next interval
            sleep(interval).await;

            // Check active hours (re-read config for dynamic reload)
            let config = self.shared_config.read().await.clone();
            if !Self::in_active_hours(&config) {
                debug!("Outside active hours, skipping heartbeat");
                emit_heartbeat_event(HeartbeatEvent {
                    ts: now_ms(),
                    status: HeartbeatStatus::Skipped,
                    duration_ms: 0,
                    preview: None,
                    reason: Some("outside active hours".to_string()),
                });
                continue;
            }

            // Run heartbeat with timing (use config already read for active_hours)
            let start = Instant::now();
            match self.run_once_internal(&config).await {
                Ok((response, status)) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let preview = if response.len() > 200 {
                        Some(format!("{}...", crate::utils::safe_truncate(&response, 200)))
                    } else {
                        Some(response.clone())
                    };

                    emit_heartbeat_event(HeartbeatEvent {
                        ts: now_ms(),
                        status,
                        duration_ms,
                        preview,
                        reason: None,
                    });

                    if is_heartbeat_ok(&response) {
                        debug!("Heartbeat: OK");
                    } else {
                        info!("Heartbeat response: {}", response);
                    }
                }
                Err(e) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    emit_heartbeat_event(HeartbeatEvent {
                        ts: now_ms(),
                        status: HeartbeatStatus::Failed,
                        duration_ms,
                        preview: None,
                        reason: Some(e.to_string()),
                    });
                    warn!("Heartbeat error: {}", e);
                }
            }
        }
    }

    /// Run a single heartbeat cycle (public API, emits events)
    pub async fn run_once(&self) -> Result<String> {
        let config = self.shared_config.read().await.clone();
        let start = Instant::now();

        match self.run_once_internal(&config).await {
            Ok((response, status)) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                let preview = if response.len() > 200 {
                    Some(format!("{}...", crate::utils::safe_truncate(&response, 200)))
                } else {
                    Some(response.clone())
                };

                emit_heartbeat_event(HeartbeatEvent {
                    ts: now_ms(),
                    status,
                    duration_ms,
                    preview,
                    reason: None,
                });

                Ok(response)
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                emit_heartbeat_event(HeartbeatEvent {
                    ts: now_ms(),
                    status: HeartbeatStatus::Failed,
                    duration_ms,
                    preview: None,
                    reason: Some(e.to_string()),
                });
                Err(e)
            }
        }
    }

    /// Internal heartbeat execution (returns response and status)
    async fn run_once_internal(&self, config: &Config) -> Result<(String, HeartbeatStatus)> {
        // Skip if an in-process agent turn is already in flight
        if let Some(ref gate) = self.turn_gate
            && gate.is_busy()
        {
            debug!("Skipping heartbeat: agent turn in flight (TurnGate busy)");
            return Ok((HEARTBEAT_OK_TOKEN.to_string(), HeartbeatStatus::Skipped));
        }

        // Try to acquire the cross-process workspace lock (non-blocking)
        let _ws_guard = match self.workspace_lock.try_acquire()? {
            Some(guard) => guard,
            None => {
                debug!("Skipping heartbeat: workspace locked by another process");
                return Ok((HEARTBEAT_OK_TOKEN.to_string(), HeartbeatStatus::Skipped));
            }
        };

        // Try to acquire the in-process turn gate (non-blocking, race between
        // the is_busy check above and now)
        let _gate_permit = if let Some(ref gate) = self.turn_gate {
            match gate.try_acquire() {
                Some(permit) => Some(permit),
                None => {
                    debug!("Skipping heartbeat: agent turn started between check and acquire");
                    return Ok((HEARTBEAT_OK_TOKEN.to_string(), HeartbeatStatus::Skipped));
                }
            }
        } else {
            None
        };

        // Check if HEARTBEAT.md exists and has content
        let workspace = config.workspace_path();
        let heartbeat_path = workspace.join("HEARTBEAT.md");

        if !heartbeat_path.exists() {
            debug!("No HEARTBEAT.md found");
            return Ok((HEARTBEAT_OK_TOKEN.to_string(), HeartbeatStatus::Skipped));
        }

        let content = fs::read_to_string(&heartbeat_path)?;
        if content.trim().is_empty() {
            debug!("HEARTBEAT.md is empty");
            return Ok((HEARTBEAT_OK_TOKEN.to_string(), HeartbeatStatus::Skipped));
        }

        // Create agent for heartbeat (clone the cached MemoryManager to share the embedding provider)
        let agent_config = AgentConfig {
            model: config.agent.default_model.clone(),
            context_window: config.agent.context_window,
            reserve_tokens: config.agent.reserve_tokens,
        };

        let mut agent = Agent::new(agent_config, config, self.memory.clone(), None).await?;
        agent.new_session().await?;

        // Check if workspace is a git repo
        let workspace_is_git = workspace.join(".git").exists();

        // Send heartbeat prompt
        let heartbeat_prompt = build_heartbeat_prompt(workspace_is_git);
        let response = agent.chat(&heartbeat_prompt).await?;

        // Determine status based on response
        if is_heartbeat_ok(&response) {
            return Ok((response, HeartbeatStatus::Ok));
        }

        // For actual alerts, check for deduplication
        let session_key = "heartbeat"; // Use dedicated session key for heartbeat state

        // Load session store to check for duplicates
        if let Ok(mut store) = SessionStore::load_for_agent(&self.agent_id) {
            if let Some(entry) = store.get(session_key)
                && entry.is_duplicate_heartbeat(&response)
            {
                debug!(
                    "Skipping duplicate heartbeat (same text within 24h): {}",
                    &response[..response.len().min(100)]
                );
                return Ok((response, HeartbeatStatus::Skipped));
            }

            // Record the heartbeat (re-read from disk to avoid clobbering)
            let session_id = agent.session_status().id;
            if let Err(e) = store.load_and_update(session_key, &session_id, |entry| {
                entry.record_heartbeat(&response);
            }) {
                warn!("Failed to record heartbeat in session store: {}", e);
            }
        }

        Ok((response, HeartbeatStatus::Sent))
    }

    fn in_active_hours(config: &Config) -> bool {
        let Some((start, end)) = config.heartbeat.active_hours.as_ref().and_then(|hours| {
            let (start_h, start_m) = parse_time(&hours.start).ok()?;
            let (end_h, end_m) = parse_time(&hours.end).ok()?;
            Some((
                NaiveTime::from_hms_opt(start_h as u32, start_m as u32, 0)?,
                NaiveTime::from_hms_opt(end_h as u32, end_m as u32, 0)?,
            ))
        }) else {
            return true; // No active hours configured, always active
        };

        let now = Local::now().time();

        if start <= end {
            // Normal range (e.g., 09:00 to 22:00)
            now >= start && now <= end
        } else {
            // Overnight range (e.g., 22:00 to 06:00)
            now >= start || now <= end
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_active_hours_normal_range() {
        // This test would require mocking Local::now()
        // For now, just verify the logic pattern
        let start = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let end = NaiveTime::from_hms_opt(22, 0, 0).unwrap();

        let noon = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();

        assert!(noon >= start && noon <= end);
        assert!(!(midnight >= start && midnight <= end));
    }
}
