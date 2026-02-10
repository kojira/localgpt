//! Append-only, hash-chained security audit log.
//!
//! Stored at `~/.localgpt/.security_audit.jsonl` (outside the workspace,
//! in the state directory). Each entry contains a SHA-256 hash of the
//! previous entry, forming a tamper-evident chain.
//!
//! # Format
//!
//! One JSON object per line (JSONL). Each entry includes:
//!
//! | Field | Description |
//! |-------|-------------|
//! | `ts` | ISO 8601 timestamp |
//! | `action` | What happened: `signed`, `verified`, `tamper_detected`, etc. |
//! | `content_sha256` | SHA-256 of the policy content at the time |
//! | `prev_entry_sha256` | SHA-256 of the previous JSONL line (chain link) |
//! | `source` | Who triggered it: `cli`, `gui`, or `session_start` |
//!
//! # Chain Integrity
//!
//! The first entry uses `000...000` (64 zeros) as `prev_entry_sha256`.
//! Every subsequent entry hashes the raw bytes of the previous line.
//! A broken chain indicates the log file was tampered with.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const AUDIT_FILENAME: &str = ".security_audit.jsonl";

/// The hash used for the first entry in the chain (no predecessor).
const GENESIS_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Security audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// ISO 8601 timestamp of the event.
    pub ts: String,
    /// What security action occurred.
    pub action: AuditAction,
    /// SHA-256 of the policy content at the time (hex-encoded).
    pub content_sha256: String,
    /// SHA-256 of the previous JSONL line (chain link, hex-encoded).
    pub prev_entry_sha256: String,
    /// Who triggered the action: `"cli"`, `"gui"`, or `"session_start"`.
    pub source: String,
}

/// Security actions recorded in the audit log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// Policy was signed via CLI or GUI.
    Signed,
    /// Policy was verified at session start.
    Verified,
    /// Tamper detected (HMAC mismatch or manifest corruption).
    TamperDetected,
    /// Policy file is missing from the workspace.
    Missing,
    /// Policy template was created during workspace init.
    Created,
    /// Policy contains suspicious prompt injection patterns.
    SuspiciousContent,
}

/// Append a new entry to the audit log.
///
/// Reads the last line of the existing log (if any) to compute the
/// chain hash, then appends a new JSONL line.
///
/// # Arguments
///
/// * `state_dir` — Path to `~/.localgpt/` (contains the audit log).
/// * `action` — What security event occurred.
/// * `content_sha256` — SHA-256 of the policy content (empty string if N/A).
/// * `source` — Who triggered the action.
pub fn append_audit_entry(
    state_dir: &Path,
    action: AuditAction,
    content_sha256: &str,
    source: &str,
) -> Result<()> {
    let path = audit_file_path(state_dir);

    // Read the last line to compute the chain hash
    let prev_hash = if path.exists() {
        let content = fs::read_to_string(&path).context("Failed to read audit log")?;
        match content.lines().last() {
            Some(last_line) if !last_line.is_empty() => sha256_hex(last_line.as_bytes()),
            _ => GENESIS_HASH.to_string(),
        }
    } else {
        GENESIS_HASH.to_string()
    };

    let entry = AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        action,
        content_sha256: content_sha256.to_string(),
        prev_entry_sha256: prev_hash,
        source: source.to_string(),
    };

    let json = serde_json::to_string(&entry).context("Failed to serialize audit entry")?;

    // Append to file (create if needed)
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open audit log")?;

    writeln!(file, "{}", json).context("Failed to write audit entry")?;

    Ok(())
}

/// Read and parse all entries from the audit log.
///
/// Returns an empty vector if the log file does not exist.
pub fn read_audit_log(state_dir: &Path) -> Result<Vec<AuditEntry>> {
    let path = audit_file_path(state_dir);

    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path).context("Failed to read audit log")?;
    let mut entries = Vec::new();

    for (i, line) in content.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let entry: AuditEntry = serde_json::from_str(line)
            .with_context(|| format!("Failed to parse audit log line {}", i + 1))?;
        entries.push(entry);
    }

    Ok(entries)
}

/// Verify the integrity of the audit log hash chain.
///
/// Returns a list of indices where the chain is broken (i.e., the
/// `prev_entry_sha256` does not match the SHA-256 of the previous line).
///
/// An empty return value means the chain is intact.
pub fn verify_audit_chain(state_dir: &Path) -> Result<Vec<usize>> {
    let path = audit_file_path(state_dir);

    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path).context("Failed to read audit log")?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();

    if lines.is_empty() {
        return Ok(Vec::new());
    }

    let mut broken = Vec::new();

    // Parse all entries
    let entries: Vec<AuditEntry> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("Failed to parse audit log line {}", i + 1))
        })
        .collect::<Result<Vec<_>>>()?;

    // Check first entry
    if entries[0].prev_entry_sha256 != GENESIS_HASH {
        broken.push(0);
    }

    // Check chain links
    for i in 1..entries.len() {
        let expected_hash = sha256_hex(lines[i - 1].as_bytes());
        if entries[i].prev_entry_sha256 != expected_hash {
            broken.push(i);
        }
    }

    Ok(broken)
}

/// Get the full path to the audit log file.
pub fn audit_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join(AUDIT_FILENAME)
}

/// Compute hex-encoded SHA-256.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_chain_integrity() {
        let tmp = tempfile::tempdir().unwrap();

        // Write 5 entries
        for i in 0..5 {
            append_audit_entry(
                tmp.path(),
                AuditAction::Verified,
                &format!("sha256_{}", i),
                "test",
            )
            .unwrap();
        }

        let entries = read_audit_log(tmp.path()).unwrap();
        assert_eq!(entries.len(), 5);

        // Chain should be intact
        let broken = verify_audit_chain(tmp.path()).unwrap();
        assert!(broken.is_empty(), "Chain should be intact: {:?}", broken);
    }

    #[test]
    fn first_entry_uses_genesis_hash() {
        let tmp = tempfile::tempdir().unwrap();
        append_audit_entry(tmp.path(), AuditAction::Created, "abc123", "cli").unwrap();

        let entries = read_audit_log(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].prev_entry_sha256, GENESIS_HASH);
        assert_eq!(entries[0].action, AuditAction::Created);
    }

    #[test]
    fn broken_chain_detected() {
        let tmp = tempfile::tempdir().unwrap();

        // Write 3 valid entries
        for i in 0..3 {
            append_audit_entry(
                tmp.path(),
                AuditAction::Verified,
                &format!("sha256_{}", i),
                "test",
            )
            .unwrap();
        }

        // Tamper with the middle line
        let path = audit_file_path(tmp.path());
        let content = fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = content.lines().collect();

        // Replace middle line with different content (breaks chain for entry 2)
        let tampered = lines[1].replace("sha256_1", "tampered_hash");
        lines[1] = &tampered;

        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let broken = verify_audit_chain(tmp.path()).unwrap();
        assert!(!broken.is_empty(), "Should detect broken chain");
        assert!(broken.contains(&2), "Entry 2 should have broken link");
    }

    #[test]
    fn empty_log_no_errors() {
        let tmp = tempfile::tempdir().unwrap();

        let entries = read_audit_log(tmp.path()).unwrap();
        assert!(entries.is_empty());

        let broken = verify_audit_chain(tmp.path()).unwrap();
        assert!(broken.is_empty());
    }

    #[test]
    fn audit_actions_serialize_snake_case() {
        let entry = AuditEntry {
            ts: "2026-02-09T14:00:00Z".to_string(),
            action: AuditAction::TamperDetected,
            content_sha256: "abc".to_string(),
            prev_entry_sha256: GENESIS_HASH.to_string(),
            source: "cli".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"tamper_detected\""));
    }
}
