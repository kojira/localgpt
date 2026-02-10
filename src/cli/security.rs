//! CLI subcommand: `localgpt security`
//!
//! Manages the workspace security policy: signing, verification,
//! audit log inspection, and security posture reporting.

use anyhow::Result;
use clap::{Args, Subcommand};

use localgpt::config::Config;
use localgpt::security;

#[derive(Args)]
pub struct SecurityArgs {
    #[command(subcommand)]
    pub command: SecurityCommands,
}

#[derive(Subcommand)]
pub enum SecurityCommands {
    /// Sign LocalGPT.md with device key
    Sign,

    /// Verify LocalGPT.md signature
    Verify,

    /// Show security audit log
    Audit {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show current security posture
    Status,
}

pub async fn run(args: SecurityArgs) -> Result<()> {
    match args.command {
        SecurityCommands::Sign => sign_policy().await,
        SecurityCommands::Verify => verify_policy().await,
        SecurityCommands::Audit { json } => show_audit(json).await,
        SecurityCommands::Status => show_status().await,
    }
}

async fn sign_policy() -> Result<()> {
    let config = Config::load()?;
    let workspace = config.workspace_path();
    let state_dir = workspace
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Workspace has no parent directory"))?;

    // Ensure device key exists
    security::ensure_device_key(state_dir)?;

    // Check policy file exists
    let policy_path = workspace.join(security::POLICY_FILENAME);
    if !policy_path.exists() {
        anyhow::bail!(
            "No {} found at {}. Create it first.",
            security::POLICY_FILENAME,
            policy_path.display()
        );
    }

    // Sign
    let manifest = security::sign_policy(state_dir, &workspace, "cli")?;

    // Write audit entry
    security::append_audit_entry(
        state_dir,
        security::AuditAction::Signed,
        &manifest.content_sha256,
        "cli",
    )?;

    println!(
        "Signed {} (sha256: {} | hmac: {})",
        security::POLICY_FILENAME,
        &manifest.content_sha256[..16],
        &manifest.hmac_sha256[..16]
    );

    Ok(())
}

async fn verify_policy() -> Result<()> {
    let config = Config::load()?;
    let workspace = config.workspace_path();
    let state_dir = workspace
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Workspace has no parent directory"))?;

    let result = security::load_and_verify_policy(&workspace, state_dir);

    match result {
        security::PolicyVerification::Valid(content) => {
            let char_count = content.len();
            println!("Policy: VALID ({} chars)", char_count);

            // Write audit entry
            let sha = security::content_sha256(&content);
            security::append_audit_entry(
                state_dir,
                security::AuditAction::Verified,
                &sha,
                "cli",
            )?;
        }
        security::PolicyVerification::Unsigned => {
            println!("Policy: UNSIGNED");
            println!("  Run `localgpt security sign` to activate.");
        }
        security::PolicyVerification::TamperDetected => {
            println!("Policy: TAMPER DETECTED");
            println!("  The file was modified after signing. Re-sign with `localgpt security sign`.");

            security::append_audit_entry(
                state_dir,
                security::AuditAction::TamperDetected,
                "",
                "cli",
            )?;
        }
        security::PolicyVerification::Missing => {
            println!("Policy: MISSING");
            println!(
                "  No {} found. Using hardcoded security only.",
                security::POLICY_FILENAME
            );
        }
        security::PolicyVerification::ManifestCorrupted => {
            println!("Policy: MANIFEST CORRUPTED");
            println!("  Re-sign with `localgpt security sign`.");
        }
        security::PolicyVerification::SuspiciousContent(warnings) => {
            println!("Policy: REJECTED (suspicious content)");
            for w in &warnings {
                println!("  - {}", w);
            }
        }
    }

    Ok(())
}

async fn show_audit(json_output: bool) -> Result<()> {
    let config = Config::load()?;
    let workspace = config.workspace_path();
    let state_dir = workspace
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Workspace has no parent directory"))?;

    let entries = security::read_audit_log(state_dir)?;

    if entries.is_empty() {
        println!("No audit log entries.");
        return Ok(());
    }

    // Verify chain integrity
    let broken = security::verify_audit_chain(state_dir)?;

    if json_output {
        let output = serde_json::to_string_pretty(&entries)?;
        println!("{}", output);
    } else {
        println!("Security Audit Log ({} entries):", entries.len());
        println!();

        for (i, entry) in entries.iter().enumerate() {
            let chain_status = if broken.contains(&i) { " [CHAIN BROKEN]" } else { "" };
            println!(
                "  {} {:?} (source: {}, sha256: {}){}",
                entry.ts,
                entry.action,
                entry.source,
                if entry.content_sha256.len() >= 16 {
                    &entry.content_sha256[..16]
                } else {
                    &entry.content_sha256
                },
                chain_status
            );
        }

        println!();
        if broken.is_empty() {
            println!("Chain integrity: INTACT");
        } else {
            println!(
                "Chain integrity: BROKEN at {} position(s)",
                broken.len()
            );
        }
    }

    Ok(())
}

async fn show_status() -> Result<()> {
    let config = Config::load()?;
    let workspace = config.workspace_path();
    let state_dir = workspace
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Workspace has no parent directory"))?;

    println!("Security Status:");

    // Policy file
    let policy_path = workspace.join(security::POLICY_FILENAME);
    if policy_path.exists() {
        let result = security::load_and_verify_policy(&workspace, state_dir);
        let status = match result {
            security::PolicyVerification::Valid(_) => "Valid (signed and verified)",
            security::PolicyVerification::Unsigned => "Unsigned (run `localgpt security sign`)",
            security::PolicyVerification::TamperDetected => "TAMPER DETECTED",
            security::PolicyVerification::Missing => "Missing",
            security::PolicyVerification::ManifestCorrupted => "Manifest corrupted",
            security::PolicyVerification::SuspiciousContent(_) => "Rejected (suspicious content)",
        };

        // Get signed_at from manifest if available
        let signed_at = security::read_manifest(&workspace)
            .ok()
            .map(|m| m.signed_at)
            .unwrap_or_else(|| "N/A".to_string());

        println!("  Policy:     {} (exists)", policy_path.display());
        println!("  Signature:  {} (signed: {})", status, signed_at);
    } else {
        println!("  Policy:     Not created");
    }

    // Device key
    let key_path = state_dir.join(".device_key");
    if key_path.exists() {
        println!("  Device Key: Present");
    } else {
        println!("  Device Key: Missing (run `localgpt init`)");
    }

    // Audit log
    let entries = security::read_audit_log(state_dir)?;
    if entries.is_empty() {
        println!("  Audit Log:  Empty");
    } else {
        let broken = security::verify_audit_chain(state_dir)?;
        let chain_status = if broken.is_empty() {
            "chain intact"
        } else {
            "CHAIN BROKEN"
        };
        println!(
            "  Audit Log:  {} entries, {}",
            entries.len(),
            chain_status
        );
    }

    // Protected files
    println!(
        "  Protected:  {} workspace files, {} external paths",
        security::PROTECTED_FILES.len(),
        security::PROTECTED_EXTERNAL_PATHS.len()
    );

    Ok(())
}
