//! launchd agent management for the spotty watch daemon.
//!
//! Installs, uninstalls, and checks status of the com.spotty.watch agent.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const AGENT_LABEL: &str = "com.spotty.watch";
const IAM_USER_NAME: &str = "spotty-watcher";
const AWS_PROFILE_NAME: &str = "spotty-watcher";

/// Get the path to the launchd agent plist file.
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join("Library/LaunchAgents").join(format!("{}.plist", AGENT_LABEL)))
}

/// Get the path to the spotty binary (current executable).
fn spotty_path() -> Result<PathBuf> {
    std::env::current_exe().context("Cannot determine spotty executable path")
}

/// Get the log file path.
fn log_path() -> Result<PathBuf> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine cache directory"))?
        .join("spotty");
    fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir.join("watch.log"))
}

/// Check if the spotty-watcher AWS profile exists.
fn has_watcher_profile() -> bool {
    let credentials_path = dirs::home_dir()
        .map(|h| h.join(".aws/credentials"))
        .unwrap_or_default();

    if !credentials_path.exists() {
        return false;
    }

    if let Ok(content) = fs::read_to_string(&credentials_path) {
        return content.contains(&format!("[{}]", AWS_PROFILE_NAME));
    }

    false
}

/// Get the configured region from spotty config or environment.
fn get_configured_region() -> Option<String> {
    // Try config file first
    if let Ok(config) = crate::config::Config::load() {
        if let Some(region) = config.defaults.region {
            return Some(region);
        }
    }
    // Fall back to environment
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .ok()
}

/// Generate the plist content for the launchd agent.
fn generate_plist() -> Result<String> {
    let spotty = spotty_path()?;
    let log = log_path()?;

    // Build environment variables section
    let has_profile = has_watcher_profile();
    let region = get_configured_region();

    let env_section = if has_profile || region.is_some() {
        let mut vars = String::new();
        if has_profile {
            vars.push_str(&format!(
                "        <key>AWS_PROFILE</key>\n        <string>{}</string>\n",
                AWS_PROFILE_NAME
            ));
        }
        if let Some(r) = region {
            vars.push_str(&format!(
                "        <key>AWS_REGION</key>\n        <string>{}</string>\n",
                r
            ));
        }
        format!(
            r#"    <key>EnvironmentVariables</key>
    <dict>
{}    </dict>
"#,
            vars
        )
    } else {
        String::new()
    };

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>watch</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
{}    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        AGENT_LABEL,
        spotty.display(),
        env_section,
        log.display(),
        log.display()
    ))
}

/// Check if the launchd agent is installed.
pub fn is_installed() -> Result<bool> {
    let path = plist_path()?;
    Ok(path.exists())
}

/// Check if the launchd agent is currently running.
pub fn is_running() -> Result<bool> {
    let output = Command::new("launchctl")
        .args(["list", AGENT_LABEL])
        .output()
        .context("Failed to run launchctl")?;

    Ok(output.status.success())
}

/// Install the launchd agent.
pub fn install() -> Result<()> {
    let path = plist_path()?;

    // Create LaunchAgents directory if needed
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Check if already installed
    if path.exists() {
        eprintln!("Agent already installed at {}", path.display());
        eprintln!("Updating configuration...");
        // Unload first if running
        let _ = Command::new("launchctl")
            .args(["unload", &path.to_string_lossy()])
            .output();
    }

    // Write plist
    let content = generate_plist()?;
    fs::write(&path, &content).with_context(|| format!("Failed to write {}", path.display()))?;

    eprintln!("Installed launchd agent: {}", path.display());

    // Load the agent
    let output = Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .output()
        .context("Failed to load agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to load agent: {}", stderr);
    }

    eprintln!("Agent loaded and running");
    eprintln!("Log file: {}", log_path()?.display());

    Ok(())
}

/// Uninstall the launchd agent.
pub fn uninstall() -> Result<()> {
    let path = plist_path()?;

    if !path.exists() {
        eprintln!("Agent not installed");
        return Ok(());
    }

    // Unload the agent
    let output = Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .output()
        .context("Failed to unload agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("Warning: Failed to unload agent: {}", stderr);
    }

    // Remove the plist file
    fs::remove_file(&path).with_context(|| format!("Failed to remove {}", path.display()))?;

    eprintln!("Agent uninstalled");

    Ok(())
}

/// Print status of the launchd agent.
pub fn status() -> Result<()> {
    let installed = is_installed()?;
    let running = is_running()?;

    println!("Agent installed: {}", if installed { "yes" } else { "no" });
    println!("Agent running:   {}", if running { "yes" } else { "no" });

    if installed {
        println!("Plist path:      {}", plist_path()?.display());
    }

    if running {
        // Get PID from launchctl list (table format)
        if let Ok(output) = Command::new("launchctl")
            .args(["list"])
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Format is: PID\tStatus\tLabel
                for line in stdout.lines() {
                    if line.ends_with(AGENT_LABEL) {
                        let parts: Vec<&str> = line.split('\t').collect();
                        if let Some(pid) = parts.first() {
                            if *pid != "-" && !pid.is_empty() {
                                println!("PID:             {}", pid);
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    println!("Log file:        {}", log_path()?.display());

    Ok(())
}

/// Ensure the agent is installed (called automatically on `spotty start`).
/// Returns true if the agent was newly installed.
pub fn ensure_installed() -> Result<bool> {
    if is_installed()? && is_running()? {
        return Ok(false);
    }

    eprintln!("Setting up auto-cleanup watcher...");
    install()?;
    Ok(true)
}

/// Print the watcher log file.
pub fn log() -> Result<()> {
    let path = log_path()?;

    if !path.exists() {
        eprintln!("Log file does not exist: {}", path.display());
        eprintln!("The watcher may not have been installed or run yet.");
        return Ok(());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    if content.is_empty() {
        eprintln!("Log file is empty: {}", path.display());
    } else {
        print!("{}", content);
    }

    Ok(())
}

/// The IAM policy that allows only terminating spotty-managed instances.
const WATCHER_POLICY: &str = r#"{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "DescribeInstances",
            "Effect": "Allow",
            "Action": "ec2:DescribeInstances",
            "Resource": "*"
        },
        {
            "Sid": "TerminateSpottyInstances",
            "Effect": "Allow",
            "Action": "ec2:TerminateInstances",
            "Resource": "*",
            "Condition": {
                "StringEquals": {
                    "ec2:ResourceTag/ManagedBy": "spotty"
                }
            }
        }
    ]
}"#;

/// Set up the watcher with dedicated IAM credentials.
/// Creates an IAM user with minimal permissions and stores credentials locally.
pub async fn setup(region: Option<String>) -> Result<()> {
    use aws_runtime_async::aws::Client;
    use crate::iam;

    eprintln!("Setting up spotty-watcher IAM user with minimal permissions...\n");

    let mut client = Client::from_env();
    if let Some(ref r) = region {
        client.creds.region = r.clone();
    }

    // Step 1: Create IAM user (or check if exists)
    eprint!("Creating IAM user '{}'... ", IAM_USER_NAME);
    let user_exists = match iam::get_user(
        &client,
        iam::GetUserRequest::default().user_name(IAM_USER_NAME),
    ).await {
        Ok(_) => {
            eprintln!("already exists");
            true
        }
        Err(e) => {
            let err_str = format!("{}", e);
            if err_str.contains("NoSuchEntity") {
                match iam::create_user(
                    &client,
                    iam::CreateUserRequest::default().user_name(IAM_USER_NAME),
                ).await {
                    Ok(_) => {
                        eprintln!("created");
                        false
                    }
                    Err(e) => {
                        bail!("Failed to create IAM user: {}", e);
                    }
                }
            } else {
                bail!("Failed to check IAM user: {}", e);
            }
        }
    };

    // Step 2: Attach inline policy
    eprint!("Attaching minimal permissions policy... ");
    iam::put_user_policy(
        &client,
        iam::PutUserPolicyRequest::default()
            .user_name(IAM_USER_NAME)
            .policy_name("spotty-watcher-policy")
            .policy_document(WATCHER_POLICY),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to attach policy")?;
    eprintln!("done");

    // Step 3: Create access keys (only if user was just created or no keys exist)
    let credentials_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".aws/credentials");

    let profile_exists = has_watcher_profile();

    if profile_exists && user_exists {
        eprintln!("AWS profile '{}' already configured", AWS_PROFILE_NAME);
    } else {
        eprint!("Creating access keys... ");

        // Delete existing access keys first (max 2 per user)
        if let Ok(keys) = iam::list_access_keys(
            &client,
            iam::ListAccessKeysRequest::default().user_name(IAM_USER_NAME),
        ).await {
            for key in &keys.access_key_metadata.item {
                if let Some(ref key_id) = key.access_key_id {
                    let _ = iam::delete_access_key(
                        &client,
                        iam::DeleteAccessKeyRequest::default()
                            .user_name(IAM_USER_NAME)
                            .access_key_id(key_id.clone()),
                    ).await;
                }
            }
        }

        // Create new access key
        let key_response = iam::create_access_key(
            &client,
            iam::CreateAccessKeyRequest::default().user_name(IAM_USER_NAME),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("Failed to create access key")?;

        let access_key = &key_response.access_key;
        let access_key_id = &access_key.access_key_id;
        let secret_key = &access_key.secret_access_key;

        eprintln!("done");

        // Step 4: Write to ~/.aws/credentials
        eprint!("Writing credentials to {}... ", credentials_path.display());
        write_aws_profile(&credentials_path, access_key_id, secret_key)?;
        eprintln!("done");
    }

    // Step 5: Reinstall the agent with AWS_PROFILE set
    eprintln!("\nReinstalling launchd agent with AWS_PROFILE={}...", AWS_PROFILE_NAME);
    install()?;

    eprintln!("\nSetup complete! The watcher now has limited permissions to:");
    eprintln!("  - Describe EC2 instances");
    eprintln!("  - Terminate instances tagged with ManagedBy=spotty");
    eprintln!("\nTo test: spotty watch log");

    Ok(())
}

/// Write or update AWS credentials profile.
fn write_aws_profile(path: &PathBuf, access_key_id: &str, secret_key: &str) -> Result<()> {
    // Ensure .aws directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read existing content
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Check if profile already exists and remove it
    let mut new_content = String::new();
    let mut skip_until_next_profile = false;
    let profile_header = format!("[{}]", AWS_PROFILE_NAME);

    for line in existing.lines() {
        if line.trim() == profile_header {
            skip_until_next_profile = true;
            continue;
        }
        if skip_until_next_profile {
            if line.starts_with('[') {
                skip_until_next_profile = false;
            } else {
                continue;
            }
        }
        new_content.push_str(line);
        new_content.push('\n');
    }

    // Add our profile
    if !new_content.is_empty() && !new_content.ends_with("\n\n") {
        new_content.push('\n');
    }
    new_content.push_str(&format!("[{}]\n", AWS_PROFILE_NAME));
    new_content.push_str(&format!("aws_access_key_id = {}\n", access_key_id));
    new_content.push_str(&format!("aws_secret_access_key = {}\n", secret_key));

    // Write back
    let mut file = fs::File::create(path)?;
    file.write_all(new_content.as_bytes())?;

    // Set permissions to 600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
