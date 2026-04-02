//! Iroh relay server — runs on an EC2 spot instance with a public IP.
//!
//! Relays encrypted traffic between the local client and the remote
//! service when direct UDP hole-punching isn't possible.

use anyhow::Result;
use clap::Parser;
use iroh_relay::server::{AccessConfig, RelayConfig, Server, ServerConfig};
use std::net::{Ipv4Addr, SocketAddr};

#[derive(Parser)]
struct Args {
    /// Port to listen on
    #[arg(long, short, default_value = "3340")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, args.port));

    eprintln!("Starting relay on {}...", addr);

    let config: ServerConfig<(), ()> = ServerConfig {
        relay: Some(RelayConfig {
            http_bind_addr: addr,
            tls: None,
            limits: Default::default(),
            access: AccessConfig::Everyone,
            key_cache_capacity: None,
        }),
        quic: None,
        metrics_addr: None,
    };

    let server = Server::spawn(config).await?;

    if let Some(addr) = server.http_addr() {
        let url = format!("http://{}:{}", addr.ip(), addr.port());
        // Print the relay URL to stdout for scripts to capture
        println!("{}", url);
        eprintln!("Relay URL: {}", url);
        eprintln!("(Replace 0.0.0.0 with your EC2 public IP)");
    }
    eprintln!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;

    Ok(())
}
