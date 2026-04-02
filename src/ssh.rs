use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::aws::client::AwsClients;
use crate::aws::instances::{get_instance_public_ip, get_instance_key_name};
use crate::config::Config;
use crate::utils::{expand_tilde, add_ssh_options, add_ssh_batch_options};

pub async fn ssh_into_instance(
    instance_id: String,
    key: Option<String>,
    user: String,
    command: Option<String>,
    region: Option<String>,
) -> Result<()> {
    let clients = AwsClients::new(region)?;
    let config = Config::load()?;

    // Get instance public IP
    let ip = get_instance_public_ip(&clients, &instance_id).await?;

    // Resolve key path: CLI arg > config > discover from instance
    let key_path = if let Some(k) = key {
        Some(expand_tilde(&k).into_owned())
    } else if let Some(ref k) = config.defaults.key_path {
        Some(expand_tilde(k).into_owned())
    } else {
        // Try to find key based on instance's key name
        let key_name = get_instance_key_name(&clients, &instance_id).await?;
        find_key_file(&key_name)
    };

    eprintln!("Connecting to {} ({})...", instance_id, ip);

    // Wait for SSH to be ready
    wait_for_ssh(&ip, &user, key_path.as_deref()).await?;

    // Build SSH command
    let mut ssh_cmd = Command::new("ssh");

    // Add key if specified
    if let Some(ref kp) = key_path {
        ssh_cmd.arg("-i").arg(kp);
    }

    // Disable strict host key checking for convenience
    add_ssh_options(&mut ssh_cmd);

    // Add user@host
    ssh_cmd.arg(format!("{}@{}", user, ip));

    // Add command if specified
    if let Some(cmd) = command {
        ssh_cmd.arg(cmd);
    }

    let status = ssh_cmd.status()?;

    if !status.success() {
        bail!("SSH exited with status: {:?}", status.code());
    }

    Ok(())
}

/// Wait for SSH to be ready on the instance
pub async fn wait_for_ssh(ip: &str, user: &str, key_path: Option<&str>) -> Result<()> {
    let max_attempts = 30;
    let delay = Duration::from_secs(2);

    for attempt in 1..=max_attempts {
        let mut cmd = Command::new("ssh");

        if let Some(kp) = key_path {
            cmd.arg("-i").arg(kp);
        }

        add_ssh_batch_options(&mut cmd);
        cmd.arg("-o").arg("ConnectTimeout=5")
            .arg(format!("{}@{}", user, ip))
            .arg("exit 0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if cmd.status().map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }

        if attempt < max_attempts {
            eprint!(".");
            tokio::time::sleep(delay).await;
        }
    }

    bail!("SSH not ready after {} attempts", max_attempts);
}


/// Run a command on an instance via SSH, capturing stdout.
pub async fn run_command(
    instance_id: &str,
    command: &str,
    region: Option<String>,
) -> Result<String> {
    let clients = AwsClients::new(region)?;
    let config = Config::load()?;

    let ip = get_instance_public_ip(&clients, instance_id).await?;

    let key_path = if let Some(ref k) = config.defaults.key_path {
        Some(expand_tilde(k).into_owned())
    } else {
        let key_name = get_instance_key_name(&clients, instance_id).await?;
        find_key_file(&key_name)
    };

    let user = config.defaults.ssh_user.as_deref().unwrap_or("ec2-user");

    wait_for_ssh(&ip, user, key_path.as_deref()).await?;

    let mut ssh_cmd = Command::new("ssh");
    if let Some(ref kp) = key_path {
        ssh_cmd.arg("-i").arg(kp);
    }
    add_ssh_options(&mut ssh_cmd);
    ssh_cmd.arg(format!("{}@{}", user, ip)).arg(command);

    let output = ssh_cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("SSH command failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Deploy local files to a running instance via SCP, optionally run a command.
pub async fn deploy_to_instance(
    instance_id: String,
    local_paths: Vec<String>,
    remote_dir: String,
    key: Option<String>,
    user: String,
    command: Option<String>,
    region: Option<String>,
) -> Result<()> {
    let clients = AwsClients::new(region)?;
    let config = Config::load()?;

    let ip = get_instance_public_ip(&clients, &instance_id).await?;

    let key_path = if let Some(k) = key {
        Some(expand_tilde(&k).into_owned())
    } else if let Some(ref k) = config.defaults.key_path {
        Some(expand_tilde(k).into_owned())
    } else {
        let key_name = get_instance_key_name(&clients, &instance_id).await?;
        find_key_file(&key_name)
    };

    // Wait for SSH to be ready
    wait_for_ssh(&ip, &user, key_path.as_deref()).await?;

    // Ensure remote directory exists
    let mut mkdir_cmd = Command::new("ssh");
    if let Some(ref kp) = key_path {
        mkdir_cmd.arg("-i").arg(kp);
    }
    add_ssh_options(&mut mkdir_cmd);
    mkdir_cmd
        .arg(format!("{}@{}", user, ip))
        .arg(format!("mkdir -p {}", remote_dir));
    let status = mkdir_cmd.status()?;
    if !status.success() {
        bail!("Failed to create remote directory: {}", remote_dir);
    }

    // SCP each file
    for local_path in &local_paths {
        let path = std::path::Path::new(local_path);
        if !path.exists() {
            bail!("Local file not found: {}", local_path);
        }
        let filename = path.file_name().unwrap().to_string_lossy();
        let target = format!("{}@{}:{}/{}", user, ip, remote_dir, filename);
        eprintln!("Uploading {} -> {}:{}/{}", local_path, instance_id, remote_dir, filename);

        let mut scp_cmd = Command::new("scp");
        if let Some(ref kp) = key_path {
            scp_cmd.arg("-i").arg(kp);
        }
        add_ssh_options(&mut scp_cmd);
        scp_cmd.arg(local_path).arg(&target);

        let status = scp_cmd.status()?;
        if !status.success() {
            bail!("SCP failed for {}", local_path);
        }
    }

    eprintln!("Deployed {} file(s) to {}:{}", local_paths.len(), instance_id, remote_dir);

    // Optionally run a command
    if let Some(cmd) = command {
        eprintln!("Running: {}", cmd);
        let mut ssh_cmd = Command::new("ssh");
        if let Some(ref kp) = key_path {
            ssh_cmd.arg("-i").arg(kp);
        }
        add_ssh_options(&mut ssh_cmd);
        ssh_cmd
            .arg(format!("{}@{}", user, ip))
            .arg(&cmd);

        let status = ssh_cmd.status()?;
        if !status.success() {
            bail!("Remote command failed with status: {:?}", status.code());
        }
    }

    Ok(())
}

fn find_key_file(key_name: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let ssh_dir = PathBuf::from(&home).join(".ssh");

    // Try common patterns
    let candidates = [
        ssh_dir.join(format!("{}.pem", key_name)),
        ssh_dir.join(key_name),
        ssh_dir.join(format!("{}.key", key_name)),
    ];

    for path in &candidates {
        if path.exists() {
            eprintln!("Using key: {}", path.display());
            return Some(path.to_string_lossy().to_string());
        }
    }

    eprintln!("Warning: Could not find key file for '{}' in ~/.ssh/", key_name);
    None
}
