use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::process::Command;
use std::time::Duration;
use tabled::Tabled;

use super::client::AwsClients;
use super::instances::{self, LaunchConfig};
use crate::config::Config;
use crate::ec2;
use crate::ssh;
use crate::utils::{add_identity, add_ssh_options};

#[derive(Debug, Serialize, Tabled)]
pub struct AmiInfo {
    #[tabled(rename = "AMI ID")]
    pub ami_id: String,
    #[tabled(rename = "Name")]
    pub name: String,
    #[tabled(rename = "Arch")]
    pub arch: String,
    #[tabled(rename = "Size")]
    pub size: String,
    #[tabled(rename = "Source")]
    pub source: String,
}

/// Look up image sizes via describe_images. Returns a map of ami_id -> size string.
async fn get_image_sizes(
    clients: &AwsClients,
    ami_ids: &[String],
) -> Result<std::collections::HashMap<String, String>> {
    let mut sizes = std::collections::HashMap::new();
    if ami_ids.is_empty() {
        return Ok(sizes);
    }

    let mut req = ec2::DescribeImagesRequest::default();
    for id in ami_ids {
        req = req.image_ids(id.clone());
    }
    let resp = ec2::describe_images(&clients.client, req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("Failed to describe images")?;

    for image in resp.images.iter().flat_map(|l| &l.item) {
        if let Some(ref id) = image.image_id {
            // Sum up all block device mapping sizes
            let total_gb: i32 = image
                .block_device_mappings
                .iter()
                .flat_map(|l| &l.item)
                .filter_map(|bdm| bdm.ebs.as_ref().and_then(|ebs| ebs.volume_size))
                .sum();
            sizes.insert(id.clone(), format!("{} GB", total_gb));
        }
    }

    Ok(sizes)
}

/// List available AMIs: base AL2023 images from SSM + spotty-created + config AMIs.
pub async fn list_images(region: Option<String>) -> Result<Vec<AmiInfo>> {
    let clients = AwsClients::new(region).await?;
    let config = Config::load().unwrap_or_default();

    let mut images: Vec<AmiInfo> = Vec::new();
    let mut ami_ids_to_lookup: Vec<String> = Vec::new();

    // Base AL2023 AMI variants to discover via SSM
    let ssm_params: &[(&str, &str, &str)] = &[
        (
            "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64",
            "x86_64",
            "AL2023",
        ),
        (
            "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64",
            "arm64",
            "AL2023",
        ),
        (
            "/aws/service/ami-amazon-linux-latest/al2023-ami-minimal-kernel-default-x86_64",
            "x86_64",
            "AL2023-minimal",
        ),
        (
            "/aws/service/ami-amazon-linux-latest/al2023-ami-minimal-kernel-default-arm64",
            "arm64",
            "AL2023-minimal",
        ),
    ];

    eprintln!("Fetching AMIs for {}...", clients.region);

    let param_names: Vec<String> = ssm_params
        .iter()
        .map(|(name, _, _)| name.to_string())
        .collect();

    let ssm_req = crate::ssm::GetParametersRequest {
        names: crate::ssm::ParameterNameList {
            item: param_names,
        },
        ..Default::default()
    };
    let resp = crate::ssm::get_parameters(&clients.client, ssm_req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
        .context("Failed to fetch SSM parameters for base AMIs")?;

    for param in resp.parameters.iter().flat_map(|l| &l.item) {
        if let (Some(ref name), Some(ref value)) = (&param.name, &param.value) {
            if let Some((_, arch, source)) = ssm_params.iter().find(|(n, _, _)| *n == name.as_str()) {
                ami_ids_to_lookup.push(value.clone());
                images.push(AmiInfo {
                    ami_id: value.clone(),
                    name: name.rsplit('/').next().unwrap_or(name).to_string(),
                    arch: arch.to_string(),
                    size: String::new(),
                    source: source.to_string(),
                });
            }
        }
    }

    // Find spotty-created AMIs (tagged ManagedBy=spotty)
    let owned_resp = ec2::describe_images(
        &clients.client,
        ec2::DescribeImagesRequest::default()
            .owners("self")
            .filters(
                ec2::Filter::default()
                    .name("tag:ManagedBy")
                    .values("spotty"),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to list spotty-created images")?;

    for image in owned_resp.images.iter().flat_map(|l| &l.item) {
        if let Some(ref id) = image.image_id {
            let name = image.name.as_deref().unwrap_or("-").to_string();
            let arch = image
                .architecture
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let total_gb: i32 = image
                .block_device_mappings
                .iter()
                .flat_map(|l| &l.item)
                .filter_map(|bdm| bdm.ebs.as_ref().and_then(|ebs| ebs.volume_size))
                .sum();

            images.push(AmiInfo {
                ami_id: id.clone(),
                name,
                arch,
                size: format!("{} GB", total_gb),
                source: "spotty".to_string(),
            });
        }
    }

    // Add custom AMIs from config
    for (key, ami_id) in &config.ami {
        // Skip if already listed as a spotty-created image
        if images.iter().any(|i| i.ami_id == *ami_id) {
            continue;
        }

        let arch = if key.ends_with("-arm64") {
            "arm64"
        } else if key.ends_with("-x86_64") {
            "x86_64"
        } else {
            "unknown"
        };

        ami_ids_to_lookup.push(ami_id.clone());
        images.push(AmiInfo {
            ami_id: ami_id.clone(),
            name: key.clone(),
            arch: arch.to_string(),
            size: String::new(),
            source: "config".to_string(),
        });
    }

    // Look up sizes for base + config AMIs
    let sizes = get_image_sizes(&clients, &ami_ids_to_lookup).await?;
    for image in &mut images {
        if image.size.is_empty() {
            if let Some(size) = sizes.get(&image.ami_id) {
                image.size = size.clone();
            }
        }
    }

    Ok(images)
}

/// Create an AMI from a running instance. Returns the AMI ID.
pub async fn create_image(
    instance_id: String,
    name: Option<String>,
    no_reboot: bool,
    wait: bool,
    region: Option<String>,
) -> Result<String> {
    let clients = AwsClients::new(region).await?;

    // Verify instance exists and get its type
    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .instance_ids(instance_id.clone()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to describe instance")?;

    let instance = resp
        .reservations
        .as_ref()
        .and_then(|l| l.item.first())
        .and_then(|r| r.instances.as_ref())
        .and_then(|l| l.item.first())
        .ok_or_else(|| anyhow::anyhow!("Instance {} not found", instance_id))?;

    let instance_type = instance
        .instance_type
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_default();

    // An image logs in as the instance it was made from did.
    let ssh_user = instance
        .tags
        .as_ref()
        .and_then(|l| l.item.iter().find(|t| t.key.as_deref() == Some(instances::SSH_USER_TAG)))
        .and_then(|t| t.value.clone());

    // Detect architecture from instance type
    let instance_types = super::filters::get_instance_types(&clients).await?;
    let arch = instance_types
        .get(&instance_type)
        .and_then(|spec| spec.architectures.first())
        .cloned()
        .unwrap_or_else(|| "x86_64".to_string());

    // Generate AMI name if not provided
    let ami_name = name.unwrap_or_else(|| {
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        format!("spotty-{}-{}-{}", instance_id, arch, timestamp)
    });

    eprintln!(
        "Creating AMI '{}' from instance {} ({})...",
        ami_name, instance_id, arch
    );

    let mut image_tags = ec2::TagSpecification::default()
        .resource_type(ec2::ResourceType::Image)
        .tags(
            ec2::Tag::default()
                .key("ManagedBy")
                .value("spotty"),
        )
        .tags(
            ec2::Tag::default()
                .key("Architecture")
                .value(arch.clone()),
        );
    if let Some(user) = ssh_user {
        image_tags = image_tags.tags(ec2::Tag::default().key(instances::SSH_USER_TAG).value(user));
    }

    let create_resp = ec2::create_image(
        &clients.client,
        ec2::CreateImageRequest::default()
            .instance_id(instance_id.clone())
            .name(ami_name)
            .no_reboot(no_reboot)
            .tag_specifications(image_tags)
            // The backing snapshots are separate resources: without their own
            // tag the scoped role cannot delete them when the AMI is removed.
            .tag_specifications(
                ec2::TagSpecification::default()
                    .resource_type(ec2::ResourceType::Snapshot)
                    .tags(
                        ec2::Tag::default()
                            .key("ManagedBy")
                            .value("spotty"),
                    ),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to create image")?;

    let ami_id = create_resp
        .image_id
        .ok_or_else(|| anyhow::anyhow!("No AMI ID returned"))?;

    if wait {
        eprintln!("Waiting for AMI to become available (this may take several minutes)...");
        wait_for_image_available(&clients, &ami_id).await?;
        eprintln!("\nAMI is available.");
    }

    Ok(ami_id)
}

/// Deregister an AMI and delete its backing snapshots.
pub async fn remove_image(ami_id: String, region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    // Get snapshot IDs before deregistering
    let resp = ec2::describe_images(
        &clients.client,
        ec2::DescribeImagesRequest::default()
            .image_ids(ami_id.clone()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to describe image")?;

    let image = resp
        .images
        .as_ref()
        .and_then(|l| l.item.first())
        .ok_or_else(|| anyhow::anyhow!("AMI {} not found", ami_id))?;

    let name = image.name.as_deref().unwrap_or("-");
    let snapshot_ids: Vec<String> = image
        .block_device_mappings
        .iter()
        .flat_map(|l| &l.item)
        .filter_map(|bdm| bdm.ebs.as_ref().and_then(|ebs| ebs.snapshot_id.clone()))
        .collect();

    eprintln!("Deregistering AMI {} ({})...", ami_id, name);
    ec2::deregister_image(
        &clients.client,
        ec2::DeregisterImageRequest::default()
            .image_id(ami_id.clone()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("Failed to deregister image")?;

    // Delete backing snapshots
    for snap_id in &snapshot_ids {
        eprintln!("Deleting snapshot {}...", snap_id);
        if let Err(e) = ec2::delete_snapshot(
            &clients.client,
            ec2::DeleteSnapshotRequest::default()
                .snapshot_id(snap_id.clone()),
        )
        .await
        {
            eprintln!("Warning: Failed to delete snapshot {}: {}", snap_id, e);
        }
    }

    eprintln!("Done.");
    Ok(())
}

/// Build an AMI by launching an instance, running a script, and baking the result.
pub async fn build_image(
    script: String,
    name: Option<String>,
    volume_size: Option<i32>,
    region: Option<String>,
    mut launch_config: LaunchConfig,
) -> Result<()> {
    // Verify script exists
    let script_path = std::path::Path::new(&script);
    if !script_path.exists() {
        bail!("Script not found: {}", script);
    }

    // Default to 20 GB if not specified
    if launch_config.volume_size.is_none() {
        launch_config.volume_size = Some(volume_size.unwrap_or(20));
    }
    launch_config.region = region.clone();

    // Step 1: Launch instance
    eprintln!("Step 1/5: Launching builder instance...");
    let instance_id = instances::start_instance_and_get_id(launch_config).await?;

    // From here on, if anything fails, terminate the instance
    match build_image_inner(&instance_id, &script, &name, &region).await {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("Build failed: {}. Terminating builder instance...", e);
            let _ = instances::stop_instances(vec![instance_id], false, region).await;
            Err(e)
        }
    }
}

async fn build_image_inner(
    instance_id: &str,
    script: &str,
    name: &Option<String>,
    region: &Option<String>,
) -> Result<()> {
    let clients = AwsClients::new(region.clone()).await?;
    let config = Config::load().unwrap_or_default();

    // Get IP and key
    let ip = instances::get_instance_public_ip(&clients, instance_id).await?;
    let key_path = ssh::resolve_key_path(&clients, &config, instance_id, None).await?;

    let user = ssh::resolve_user(&clients, &config, instance_id, None).await?;
    let user = user.as_str();

    // Step 2: Wait for SSH
    eprintln!("Step 2/5: Waiting for SSH...");
    ssh::wait_for_ssh(&ip, user, key_path.as_deref()).await?;
    eprintln!(" ready.");

    // Step 3: Upload and run script
    eprintln!("Step 3/5: Uploading and running script...");
    let target = format!("{}@{}:/tmp/spotty-build.sh", user, ip);

    // SCP upload
    let mut scp_cmd = Command::new("scp");
    if let Some(ref kp) = key_path {
        add_identity(&mut scp_cmd, kp);
    }
    add_ssh_options(&mut scp_cmd);
    scp_cmd.arg(script).arg(&target);

    let scp_status = scp_cmd.status().context("Failed to run scp")?;
    if !scp_status.success() {
        bail!("SCP failed with status: {:?}", scp_status.code());
    }

    // SSH run script
    let mut ssh_cmd = Command::new("ssh");
    if let Some(ref kp) = key_path {
        add_identity(&mut ssh_cmd, kp);
    }
    add_ssh_options(&mut ssh_cmd);
    ssh_cmd
        .arg(format!("{}@{}", user, ip))
        // The image is taken without a reboot, so whatever the script wrote
        // that is still in the page cache would be missing from it: sync.
        .arg("chmod +x /tmp/spotty-build.sh && /tmp/spotty-build.sh && sync");

    let ssh_status = ssh_cmd.status().context("Failed to run ssh")?;
    if !ssh_status.success() {
        bail!(
            "Build script failed with exit code: {:?}",
            ssh_status.code()
        );
    }

    eprintln!("Script completed successfully.");

    // Step 4: Bake AMI
    eprintln!("Step 4/5: Creating AMI...");
    create_image(
        instance_id.to_string(),
        name.clone(),
        true,  // no_reboot
        true,  // wait
        region.clone(),
    )
    .await?;

    // Step 5: Terminate builder instance
    eprintln!("Step 5/5: Terminating builder instance...");
    instances::stop_instances(vec![instance_id.to_string()], false, region.clone()).await?;

    eprintln!("Image build complete.");
    Ok(())
}

/// Find an SSH key file in ~/.ssh/ by key name.
/// Poll DescribeImages until the image is available.
async fn wait_for_image_available(clients: &AwsClients, ami_id: &str) -> Result<()> {
    loop {
        let resp = ec2::describe_images(
            &clients.client,
            ec2::DescribeImagesRequest::default()
                .image_ids(ami_id),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        let state = resp
            .images
            .as_ref()
            .and_then(|l| l.item.first())
            .and_then(|img| img.state.as_ref())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        match state.as_str() {
            "available" => return Ok(()),
            "pending" => {
                eprint!(".");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            "failed" | "error" | "deregistered" => {
                bail!("AMI entered unexpected state: {}", state);
            }
            _ => {
                eprint!(".");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
