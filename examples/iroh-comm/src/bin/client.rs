//! Local client — runs on your machine.
//!
//! Connects to the remote service via the relay and sends requests.

use anyhow::Result;
use clap::Parser;
use iroh::endpoint::RelayMode;
use iroh::{Endpoint, EndpointAddr, PublicKey};

const ALPN: &[u8] = b"spotty-comm/v1";

#[derive(Parser)]
struct Args {
    /// Relay URL (e.g. http://<ec2-ip>:3340)
    #[arg(long, short)]
    relay: String,

    /// Service node ID (printed by the service binary)
    #[arg(long, short)]
    node_id: String,

    /// Message to send
    #[arg(default_value = "hello from local")]
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let relay_url: iroh::RelayUrl = args.relay.parse()?;

    let remote_id: PublicKey = args.node_id.parse()
        .map_err(|e| anyhow::anyhow!("Invalid node ID: {}", e))?;

    let endpoint = Endpoint::empty_builder()
        .relay_mode(RelayMode::custom([relay_url.clone()]))
        .bind()
        .await?;

    endpoint.online().await;

    // Build remote address with relay hint
    let remote_addr = EndpointAddr::new(remote_id)
        .with_relay_url(relay_url);

    eprintln!("Connecting to {}...", remote_id);
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
