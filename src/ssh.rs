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
    let clients = AwsClients::new(region).await?;
    let config = Config::load()?;

    // Get instance public IP
    let ip = get_instance_public_ip(&clients, &instance_id).await?;

    let key_path = resolve_key_path(&clients, &config, &instance_id, key).await?;

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

/// Wait for SSH to be ready on the instance.
///
/// A fresh instance refuses connections for a while, so most failures here are
/// worth retrying. Some are not — a key OpenSSH cannot even load will fail the
/// same way on attempt 30 as on attempt 1 — and those are reported at once
/// rather than after a minute of dots that suggest the instance is still
/// booting.
pub async fn wait_for_ssh(ip: &str, user: &str, key_path: Option<&str>) -> Result<()> {
    let max_attempts = 30;
    let delay = Duration::from_secs(2);
    let mut dotted = false;
    let mut last_error = String::new();

    for attempt in 1..=max_attempts {
        match ssh_probe(ip, user, key_path) {
            Ok(()) => {
                if dotted {
                    eprintln!();
                }
                return Ok(());
            }
            Err(stderr) => {
                if let Some(reason) = fatal_ssh_error(&stderr, attempt) {
                    if dotted {
                        eprintln!();
                    }
                    bail!("cannot SSH to {user}@{ip}: {reason}");
                }
                last_error = stderr;
            }
        }

        if attempt < max_attempts {
            eprint!(".");
            dotted = true;
            tokio::time::sleep(delay).await;
        }
    }

    if dotted {
        eprintln!();
    }
    bail!(
        "SSH not ready after {} attempts ({}s). Last error: {}",
        max_attempts,
        u64::from(max_attempts) * delay.as_secs(),
        summarize_ssh_error(&last_error),
    );
}

/// One non-interactive SSH attempt. `Err` carries ssh's own stderr.
fn ssh_probe(ip: &str, user: &str, key_path: Option<&str>) -> Result<(), String> {
    let mut cmd = Command::new("ssh");

    if let Some(kp) = key_path {
        cmd.arg("-i").arg(kp);
    }

    add_ssh_batch_options(&mut cmd);
    cmd.arg("-o").arg("ConnectTimeout=5")
        .arg(format!("{}@{}", user, ip))
        .arg("exit 0")
        .stdout(Stdio::null());

    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => Err(format!("could not run ssh: {e}")),
    }
}

/// Waiting will not help — the reason to report, or `None` to keep retrying.
///
/// `attempt` matters for one case: cloud-init can install the authorized key
/// just after sshd starts accepting connections, so a rejected key is only
/// conclusive once the instance has had a moment to finish.
fn fatal_ssh_error(stderr: &str, attempt: u32) -> Option<String> {
    // Nothing about the instance can fix these: the key is unreadable, absent,
    // or the host identity is wrong.
    const ALWAYS: &[&str] = &[
        "error in libcrypto",
        "invalid format",
        "Load key",
        "no such identity",
        "bad permissions",
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "Could not resolve hostname",
        "could not run ssh",
    ];
    // The server answered and turned us away. Real, but give cloud-init a
    // few seconds to write authorized_keys before believing it.
    const ONCE_SETTLED: &[&str] = &["Permission denied", "Too many authentication failures"];

    let matched = ALWAYS.iter().any(|p| stderr.contains(p))
        || (attempt > 3 && ONCE_SETTLED.iter().any(|p| stderr.contains(p)));

    matched.then(|| summarize_ssh_error(stderr))
}

/// The useful line out of ssh's stderr, dropping banners and debug chatter.
fn summarize_ssh_error(stderr: &str) -> String {
    let interesting: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("debug") && !l.starts_with("Warning: Permanently added"))
        .collect();

    if interesting.is_empty() {
        return "no output from ssh".to_string();
    }
    // The last line is ssh's verdict; anything before it is usually the cause
    // ("Load key ...: error in libcrypto" then "Permission denied").
    let verdict = interesting[interesting.len() - 1];
    match interesting.len() {
        1 => verdict.to_string(),
        _ => format!("{verdict} ({})", interesting[..interesting.len() - 1].join("; ")),
    }
}


/// Run a command on an instance via SSH, capturing stdout.
pub async fn run_command(
    instance_id: &str,
    command: &str,
    region: Option<String>,
) -> Result<String> {
    let clients = AwsClients::new(region).await?;
    let config = Config::load()?;

    let ip = get_instance_public_ip(&clients, instance_id).await?;

    let key_path = resolve_key_path(&clients, &config, instance_id, None).await?;

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
    let clients = AwsClients::new(region).await?;
    let config = Config::load()?;

    let ip = get_instance_public_ip(&clients, &instance_id).await?;

    let key_path = resolve_key_path(&clients, &config, &instance_id, key).await?;

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

/// The private key to use for `instance_id`.
///
/// The key pair the instance was actually launched with outranks the config,
/// because it is the only one that can work: its public key was baked into the
/// instance at boot, and a config written later — or by `setup` on another day
/// — may well name a different one.
pub async fn resolve_key_path(
    clients: &AwsClients,
    config: &Config,
    instance_id: &str,
    explicit: Option<String>,
) -> Result<Option<String>> {
    if let Some(k) = explicit {
        return Ok(Some(expand_tilde(&k).into_owned()));
    }

    let configured = config
        .region_defaults(&clients.region)
        .key_path
        .map(|k| expand_tilde(&k).into_owned());

    // If the instance cannot be queried, configuration is all there is.
    let Ok(key_name) = get_instance_key_name(clients, instance_id).await else {
        return Ok(configured);
    };

    if let Some(path) = find_key_file(&key_name) {
        eprintln!("Using key: {path}");
        return Ok(Some(path));
    }

    match configured {
        Some(path) => {
            eprintln!(
                "Warning: {instance_id} was launched with key pair '{key_name}', which has no \
                 private key in ~/.ssh; falling back to the configured {path}."
            );
            Ok(Some(path))
        }
        None => {
            eprintln!("Warning: Could not find key file for '{key_name}' in ~/.ssh/");
            Ok(None)
        }
    }
}

/// Locate the private key for an EC2 key pair in `~/.ssh`, quietly.
///
/// Also answers "can this machine actually use that key pair?" — AWS hands out
/// the private key only at creation, so a key pair created on another machine
/// has no file here.
pub fn find_key_file(key_name: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let ssh_dir = PathBuf::from(&home).join(".ssh");

    // Try common patterns
    let candidates = [
        ssh_dir.join(format!("{}.pem", key_name)),
        ssh_dir.join(key_name),
        ssh_dir.join(format!("{}.key", key_name)),
    ];

    candidates
        .iter()
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}

