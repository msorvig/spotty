//! Local client — runs on your machine.
//!
//! Uses spotty as a library to provision an EC2 relay instance,
//! deploy the relay binary, then connects to the remote service via iroh.

use anyhow::{bail, Result};
use clap::Parser;
use iroh::endpoint::RelayMode;
use iroh::{Endpoint, EndpointAddr, PublicKey};
use spotty::aws::instances::{self, LaunchConfig};
use spotty::ssh;

const ALPN: &[u8] = b"spotty-comm/v1";
const RELAY_PORT: u16 = 3340;

#[derive(Parser)]
struct Args {
    /// Service node ID (printed by the service binary)
    #[arg(long, short)]
    node_id: String,

    /// Path to the relay binary to deploy
    #[arg(long, default_value = "target/x86_64-unknown-linux-gnu/release/relay")]
    relay_binary: String,

    /// Message to send
    #[arg(default_value = "hello from local")]
    message: String,

    /// AWS region
    #[arg(long)]
    region: Option<String>,
}

/// Ensure a relay instance is running. Returns the relay URL.
async fn ensure_relay(relay_binary: &str, region: Option<String>) -> Result<String> {
    // Check for an existing running instance
    let running = instances::get_running_instances(region.clone()).await?;
    let relay_instance = running.iter().find(|i| i.name == "relay");

    let (instance_id, public_ip) = if let Some(inst) = relay_instance {
        eprintln!("Using existing relay instance: {}", inst.instance_id);
        let ip = inst.public_ip.clone()
            .ok_or_else(|| anyhow::anyhow!("Relay instance has no public IP"))?;
        (inst.instance_id.clone(), ip)
    } else {
        // Launch a small instance for the relay
        eprintln!("Launching relay instance...");
        let config = LaunchConfig {
            instance_type: Some("t3.micro".to_string()),
            name: Some("relay".to_string()),
            region: region.clone(),
            no_auto_cleanup: true,
            max_tries: 5,
            ..Default::default()
        };
        let infos = instances::start_instances(config, 1, true).await?;
        let info = infos.into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("Failed to launch relay instance"))?;
        if info.public_ip == "-" {
            bail!("Relay instance has no public IP");
        }
        (info.instance_id, info.public_ip)
    };

    // Open relay port on security group
    instances::open_port(RELAY_PORT, region.clone()).await?;

    // Deploy relay binary
    eprintln!("Deploying relay binary...");
    ssh::deploy_to_instance(
        instance_id.clone(),
        vec![relay_binary.to_string()],
        "~/app".to_string(),
        None,
        "ec2-user".to_string(),
        Some(format!(
            "chmod +x ~/app/relay && \
             (pgrep -f 'app/relay' > /dev/null 2>&1 || \
             nohup ~/app/relay --port {} > /tmp/relay.log 2>&1 &)",
            RELAY_PORT
        )),
        region,
    ).await?;

    let relay_url = format!("http://{}:{}", public_ip, RELAY_PORT);
    eprintln!("Relay ready: {}", relay_url);
    Ok(relay_url)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let remote_id: PublicKey = args.node_id.parse()
        .map_err(|e| anyhow::anyhow!("Invalid node ID: {}", e))?;

    // Provision relay
    let relay_url_str = ensure_relay(&args.relay_binary, args.region).await?;
    let relay_url: iroh::RelayUrl = relay_url_str.parse()?;

    // Connect to service via iroh
    let endpoint = Endpoint::empty_builder()
        .relay_mode(RelayMode::custom([relay_url.clone()]))
        .bind()
        .await?;

    endpoint.online().await;

    let remote_addr = EndpointAddr::new(remote_id)
        .with_relay_url(relay_url);

    eprintln!("Connecting to service {}...", remote_id);
    let connection = endpoint.connect(remote_addr, ALPN).await?;
    eprintln!("Connected!");

    // Send request
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(args.message.as_bytes()).await?;
    send.finish()?;

    // Read response
    let response = recv.read_to_end(64 * 1024).await?;
    let response_str = String::from_utf8_lossy(&response);
    println!("{}", response_str);

    // Clean shutdown
    connection.close(0u32.into(), b"done");
    endpoint.close().await;

    Ok(())
}
