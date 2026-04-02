# iroh-comm

Example showing how to set up communication between a local process and
a remote service using [iroh](https://docs.rs/iroh) peer-to-peer
networking, deployed with spotty.

## Architecture

```
Local machine           EC2 spot instance (public IP)
+--------+             +-------+     +----------+
| client | <--iroh-->  | relay | <-- | service  |
+--------+             +-------+     +----------+
```

Three binaries:
- **relay** — iroh relay server on EC2 with public IP. Routes encrypted
  traffic between client and service.
- **service** — the work process. Connects to the relay, accepts
  connections, handles requests. Runs on EC2 or Lambda.
- **client** — local process. Connects to service through the relay.

When using Lambda, the relay runs on a small EC2 spot instance and the
service binary runs as a Lambda function:

```
Local machine       EC2 (small)     Lambda
+--------+         +-------+       +----------+
| client | <-iroh->| relay | <---> | service  |
+--------+         +-------+       +----------+
```

## Build

Cross-compile all three binaries for Linux:

```bash
cargo build --release --target x86_64-unknown-linux-gnu
```

## Deploy with spotty

### EC2: relay + service on one instance

```bash
# Launch an instance
spotty start --arch x86_64 --wait

# Deploy relay and service
spotty deploy \
  target/x86_64-unknown-linux-gnu/release/relay \
  target/x86_64-unknown-linux-gnu/release/service \
  --to ~/app \
  --run "chmod +x ~/app/relay ~/app/service"

# SSH in and start the relay (note the public IP from spotty status)
spotty ssh -c "~/app/relay --port 3340 &"

# Start the service (it prints its node ID)
spotty ssh -c "~/app/service --relay http://<PUBLIC_IP>:3340"
```

### Lambda: relay on EC2, service on Lambda

```bash
# Launch a small relay instance
spotty start --instance-type t3.micro --wait --name relay

# Deploy and start the relay
spotty deploy target/x86_64-unknown-linux-gnu/release/relay --to ~/app \
  --run "chmod +x ~/app/relay && nohup ~/app/relay --port 3340 > /tmp/relay.log 2>&1 &"

# Deploy service to Lambda
spotty lambda deploy target/x86_64-unknown-linux-gnu/release/service

# Invoke the Lambda service (it connects to relay on startup)
spotty lambda start spotty-service \
  --payload '{"relay": "http://<RELAY_IP>:3340"}'
```

## Run locally (client)

```bash
# Run the client (use the node ID printed by the service)
cargo run --bin client -- \
  --relay http://<EC2_PUBLIC_IP>:3340 \
  --node-id <SERVICE_NODE_ID> \
  "hello from my machine"
```

## Security

All traffic between client and service is end-to-end encrypted by iroh
(QUIC/TLS). The relay cannot read the content — it only routes encrypted
packets. Endpoints are authenticated by Ed25519 public keys.
