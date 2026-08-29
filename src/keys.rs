use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::aws::client::AwsClients;
use crate::ec2;

fn ssh_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join(".ssh"))
}

pub async fn create_key_pair(name: &str, region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    // Check if key already exists in AWS
    let existing = ec2::describe_key_pairs(
        &clients.client,
        ec2::DescribeKeyPairsRequest::default().key_names(name),
    )
    .await;

    if let Ok(ref resp) = existing {
        let has_keys = resp.key_pairs.as_ref().map_or(false, |l| !l.item.is_empty());
        if has_keys {
            bail!("Key pair '{}' already exists in AWS", name);
        }
    }

    // Check if local file already exists
    let key_path = ssh_dir()?.join(format!("{}.pem", name));
    if key_path.exists() {
        bail!("Key file already exists: {}", key_path.display());
    }

    // Create the key pair
    let resp = ec2::create_key_pair(
        &clients.client,
        ec2::CreateKeyPairRequest::default()
            .key_name(name)
            .key_type(ec2::KeyType::Ed25519),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let private_key = resp
        .key_material
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No key material returned"))?;

    // OpenSSH rejects a key whose final -----END----- line has no newline
    // after it ("error in libcrypto"), and the XML parser strips the trailing
    // whitespace off every text node, so what AWS sent arrives one byte short.
    let mut private_key = private_key.clone();
    if !private_key.ends_with('\n') {
        private_key.push('\n');
    }

    crate::utils::write_secret(&key_path, &private_key)?;

    println!("Created key pair: {}", name);
    println!("Private key saved to: {}", key_path.display());

    Ok(())
}

pub async fn list_key_pairs(region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    let resp = ec2::describe_key_pairs(
        &clients.client,
        ec2::DescribeKeyPairsRequest::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let ssh = ssh_dir()?;

    println!("{:<30} {:<15} {}", "NAME", "TYPE", "LOCAL FILE");
    println!("{}", "-".repeat(60));

    for key in resp.key_pairs.iter().flat_map(|l| &l.item) {
        let name = key.key_name.as_deref().unwrap_or("-");
        let key_type = key
            .key_type
            .as_ref()
            .map(|t| format!("{:?}", t))
            .unwrap_or_else(|| "-".to_string());

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
