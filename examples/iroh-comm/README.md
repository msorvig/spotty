# iroh-comm

Example showing how to set up communication between a local process and
a remote service using [iroh](https://docs.rs/iroh) peer-to-peer
networking, with infrastructure managed by spotty as a library.

## Architecture

```
Local machine           EC2 spot instance (public IP)
+--------+             +-------+     +----------+
| client | <--iroh-->  | relay | <-- | service  |
+--------+             +-------+     +----------+
  uses spotty lib to
  provision the relay
```

Three binaries:
- **relay** — iroh relay server, deployed to EC2 by the client.
- **service** — accepts connections via the relay. Runs on EC2 or Lambda.
- **client** — local process. Uses spotty as a library to automatically
  provision the relay instance, then connects to the service via iroh.

The client handles all infrastructure setup programmatically:
1. Launches an EC2 spot instance (or reuses an existing one named "relay")
2. Opens the relay port on the security group
3. Deploys and starts the relay binary
4. Connects to the service through the relay

## Build

Cross-compile the relay and service for Linux (requires
[zig](https://ziglang.org/) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)):

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu
```

Build the client for your local machine:

```bash
cargo build --release --bin client
```

## Run

Start the service on EC2 (or run it locally for testing):

```bash
# Deploy service to an EC2 instance
spotty start --arch x86_64 --wait
spotty deploy target/x86_64-unknown-linux-gnu/release/service --to ~/app \
  --run "chmod +x ~/app/service"

# Start the service (it prints its node ID)
spotty ssh -c "~/app/service --relay http://<RELAY_IP>:3340"
```

Run the client — it provisions the relay automatically:

```bash
# The client launches/reuses an EC2 relay, deploys the relay binary, then connects
./target/release/client \
  --node-id <SERVICE_NODE_ID> \
  "hello from my machine"
```

## How it works

The client uses spotty's library API directly:

```rust
use spotty::aws::instances::{self, LaunchConfig};
use spotty::ssh;

// Launch a relay instance
let config = LaunchConfig {
    instance_type: Some("t3.micro".into()),
    name: Some("relay".into()),
    ..Default::default()
};
let infos = instances::start_instances(config, 1, true).await?;

// Open the relay port
instances::open_port(3340, None).await?;

// Deploy and start the relay binary
ssh::deploy_to_instance(
    instance_id, vec!["./relay".into()], "~/app".into(),
    None, "ec2-user".into(),
    Some("nohup ~/app/relay --port 3340 &".into()),
    None,
).await?;
```

## Security

All traffic between client and service is end-to-end encrypted by iroh
(QUIC/TLS). The relay cannot read the content — it only routes encrypted
packets. Endpoints are authenticated by Ed25519 public keys.
