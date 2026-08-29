use anyhow::{bail, Context, Result};
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

/// Delete a key pair from AWS and remove its private key from `~/.ssh`.
///
/// Refuses while an instance is still using the key pair: deleting the local
/// private key would lock you out of a machine that is running and billing.
/// The AWS-side key pair is only a stored public key, so removing it never
/// affects an instance that is already up — only future launches by that name.
pub async fn remove_key_pair(name: &str, region: Option<String>, force: bool) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    let in_use = instances_using_key(&clients, name).await?;
    if !in_use.is_empty() && !force {
        bail!(
            "key pair '{}' is still in use by {}. Terminate {} first, or pass --force to \
             delete the key anyway (you will not be able to SSH in afterwards).",
            name,
            in_use.join(", "),
            if in_use.len() == 1 { "it" } else { "them" },
        );
    }
    if !in_use.is_empty() {
        eprintln!("Warning: {} still uses '{}'.", in_use.join(", "), name);
    }

    ec2::delete_key_pair(
        &clients.client,
        ec2::DeleteKeyPairRequest::default().key_name(name),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .with_context(|| format!("failed to delete key pair '{name}' in {}", clients.region))?;
    println!("Deleted key pair '{}' in {}.", name, clients.region);

    // DeleteKeyPair succeeds whether or not the key pair existed, so say
    // plainly what happened to the local file rather than implying more.
    let local = ssh_dir()?.join(format!("{name}.pem"));
    if local.exists() {
        std::fs::remove_file(&local)?;
        println!("Removed {}.", local.display());
    } else {
        println!("No local private key at {}.", local.display());
    }

    Ok(())
}

/// Non-terminated instances launched with `key_name`.
async fn instances_using_key(clients: &AwsClients, key_name: &str) -> Result<Vec<String>> {
    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .filters(ec2::Filter::default().name("key-name").values(key_name))
            .filters(
                ec2::Filter::default()
                    .name("instance-state-name")
                    .values("pending")
                    .values("running")
                    .values("stopping")
                    .values("stopped"),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("failed to check whether the key pair is in use")?;

    Ok(resp
        .reservations
        .iter()
        .flat_map(|l| &l.item)
        .filter_map(|r| r.instances.as_ref())
        .flat_map(|l| &l.item)
        .filter_map(|i| i.instance_id.clone())
        .collect())
}
