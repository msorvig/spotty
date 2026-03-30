use anyhow::{bail, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::aws::client::AwsClients;

fn ssh_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join(".ssh"))
}

pub async fn create_key_pair(name: &str, region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    // Check if key already exists in AWS
    let existing = clients
        .ec2
        .describe_key_pairs()
        .key_names(name)
        .send()
        .await;

    if existing.is_ok() && !existing.as_ref().unwrap().key_pairs().is_empty() {
        bail!("Key pair '{}' already exists in AWS", name);
    }

    // Check if local file already exists
    let key_path = ssh_dir()?.join(format!("{}.pem", name));
    if key_path.exists() {
        bail!("Key file already exists: {}", key_path.display());
    }

    // Create the key pair
    let resp = clients
        .ec2
        .create_key_pair()
        .key_name(name)
        .key_type(aws_sdk_ec2::types::KeyType::Ed25519)
        .send()
        .await?;

    let private_key = resp
        .key_material()
        .ok_or_else(|| anyhow::anyhow!("No key material returned"))?;

    // Ensure .ssh directory exists
    let ssh = ssh_dir()?;
    fs::create_dir_all(&ssh)?;

    // Write the private key
    fs::write(&key_path, private_key)?;

    // Set permissions to 600
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;

    println!("Created key pair: {}", name);
    println!("Private key saved to: {}", key_path.display());

    Ok(())
}

pub async fn list_key_pairs(region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    let resp = clients.ec2.describe_key_pairs().send().await?;

    let ssh = ssh_dir()?;

    println!("{:<30} {:<15} {}", "NAME", "TYPE", "LOCAL FILE");
    println!("{}", "-".repeat(60));

    for key in resp.key_pairs() {
        let name = key.key_name().unwrap_or("-");
        let key_type = key
            .key_type()
            .map(|t| t.as_str())
            .unwrap_or("-");

        // Check for local key file
        let local_status = [
            ssh.join(format!("{}.pem", name)),
            ssh.join(name),
        ]
        .iter()
        .find(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".to_string());

        println!("{:<30} {:<15} {}", name, key_type, local_status);
    }

    Ok(())
}
