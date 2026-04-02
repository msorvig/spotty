//! Service binary — runs on EC2 spot instance or Lambda.
//!
//! Connects to the relay, accepts connections from the local client,
//! and handles requests. Prints its node ID so the client can connect.

use anyhow::Result;
use clap::Parser;
use iroh::endpoint::RelayMode;
use iroh::Endpoint;

const ALPN: &[u8] = b"spotty-comm/v1";

#[derive(Parser)]
struct Args {
    /// Relay URL (e.g. http://<ec2-ip>:3340)
    #[arg(long, short)]
    relay: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let relay_url: iroh::RelayUrl = args.relay.parse()?;

    let endpoint = Endpoint::empty_builder()
        .relay_mode(RelayMode::custom([relay_url]))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    // Wait until connected to relay
    endpoint.online().await;

    let node_id = endpoint.id();

    eprintln!("Service online");
    eprintln!("Node ID: {}", node_id);
    eprintln!("Waiting for connections...");

    // Print node ID to stdout for easy capture
    println!("{}", node_id);

    // Accept loop
    while let Some(incoming) = endpoint.accept().await {
        let connection = incoming.await?;
        let remote = connection.remote_id();
        eprintln!("Connection from: {}", remote);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(connection).await {
                eprintln!("Connection error: {}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(connection: iroh::endpoint::Connection) -> Result<()> {
    loop {
        let (mut send, mut recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(_) => break, // connection closed
        };

        // Read request
        let data = recv.read_to_end(64 * 1024).await?;
        let request = String::from_utf8_lossy(&data);
        eprintln!("Received: {}", request);

        // Echo back with prefix
        let response = format!("OK: {}", request);
        send.write_all(response.as_bytes()).await?;
        send.finish()?;
    }

    Ok(())
}
