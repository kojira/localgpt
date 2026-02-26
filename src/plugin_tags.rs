//! Plugin tag execution shared between Discord and Voice pipelines.
//!
//! Handles `[TAGNAME:content]` patterns defined in `[tags.*]` config sections.

use std::collections::HashMap;
use regex::Regex;
use tracing::{error, info, warn};

use crate::config::TagGroup;

/// Execute all `[TAGNAME:content]` command tags found in `response`.
/// Tag group names come from the config HashMap keys (case-insensitive).
/// Fire-and-forget: errors are logged, not propagated.
pub async fn execute_command_tags(response: &str, tags: &HashMap<String, TagGroup>) {
    if tags.is_empty() {
        return;
    }
    let names: Vec<String> = tags.keys().map(|k| k.to_uppercase()).collect();
    let pattern = format!(r"\[({}):([^\]]+)\]", names.join("|"));
    let tag_re = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to compile tag regex: {}", e);
            return;
        }
    };

    for cap in tag_re.captures_iter(response) {
        let tag_name = &cap[1];
        let content = &cap[2];

        let group = match tags.get(&tag_name.to_lowercase()) {
            Some(g) => g,
            None => continue,
        };

        match match_command_template(content, &group.patterns, group.binary.as_deref()) {
            Some(cmd) => run_command(group.config_swap.as_deref(), &cmd).await,
            None => warn!("Unknown {} command: {}", tag_name, content),
        }
    }
}

/// Match tag content against a group's configured patterns and return the expanded command.
pub fn match_command_template(
    tag_content: &str,
    patterns: &HashMap<String, String>,
    binary: Option<&str>,
) -> Option<String> {
    let tag_parts: Vec<&str> = tag_content.splitn(20, ':').collect();
    for (pattern, template) in patterns {
        let pattern_parts: Vec<&str> = pattern.splitn(20, ':').collect();
        if let Some(bindings) = match_pattern(&pattern_parts, &tag_parts) {
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
/// The last placeholder greedily captures all remaining segments.
pub fn match_pattern(pattern_parts: &[&str], tag_parts: &[&str]) -> Option<Vec<(String, String)>> {
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

/// Run a command, optionally swapping a config file around the execution.
pub async fn run_command(config_swap: Option<&str>, command: &str) {
    if let Some(config_dir) = config_swap {
        let config_dir_expanded = shellexpand::tilde(config_dir).to_string();
        let nostaro_dir = shellexpand::tilde("~/.nostaro").to_string();
        let target_config = format!("{}/config.toml", nostaro_dir);
        let source_config = format!("{}/config.toml", config_dir_expanded);

        if tokio::fs::metadata(&source_config).await.is_err() {
            error!("Config swap source not found: {}", source_config);
            return;
        }
        if let Err(e) = tokio::fs::create_dir_all(&nostaro_dir).await {
            error!("Failed to create dir {}: {}", nostaro_dir, e);
            return;
        }

        let original_exists = tokio::fs::metadata(&target_config).await.is_ok();
        let backup_path = format!("{}.localgpt-backup", target_config);

        if original_exists {
            if let Err(e) = tokio::fs::copy(&target_config, &backup_path).await {
                error!("Failed to backup config: {}", e);
                return;
            }
        }
        if let Err(e) = tokio::fs::copy(&source_config, &target_config).await {
            error!("Failed to copy config: {}", e);
            if original_exists {
                let _ = tokio::fs::rename(&backup_path, &target_config).await;
            }
            return;
        }

        info!("Executing command (config swap): {}", command);
        let result = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await;
        match result {
            Ok(output) => {
                if output.status.success() {
                    info!("Command success: {}", String::from_utf8_lossy(&output.stdout).trim());
                } else {
                    error!("Command failed (exit {}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim());
                }
            }
            Err(e) => error!("Failed to execute command: {}", e),
        }

        if original_exists {
            if let Err(e) = tokio::fs::rename(&backup_path, &target_config).await {
                error!("Failed to restore config backup: {}", e);
            }
        } else if let Err(e) = tokio::fs::remove_file(&target_config).await {
            error!("Failed to remove swapped config: {}", e);
        }
    } else {
        info!("Executing command: {}", command);
        let result = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await;
        match result {
            Ok(output) => {
                if output.status.success() {
                    info!("Command success: {}", String::from_utf8_lossy(&output.stdout).trim());
                } else {
                    error!("Command failed (exit {}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim());
                }
            }
            Err(e) => error!("Failed to execute command: {}", e),
        }
    }
}
