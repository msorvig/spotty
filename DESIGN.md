# Spotty Design Document

## Overview

Spotty is a Rust CLI tool for managing AWS EC2 spot instances. It provides a streamlined workflow for developers who need temporary compute resources at low cost.

## Architecture

Spotty is structured as a library crate with a binary frontend. The library (`lib.rs`) provides all functionality, while the binary (`main.rs`) handles CLI parsing.

```
┌─────────────────────────────────────────────────────────────┐
│                         main.rs (binary)                     │
│                    CLI parsing with clap                     │
│              Uses InstanceFilterArgs (flattened)             │
└─────────────────────────┬───────────────────────────────────┘
                          │
┌─────────────────────────▼───────────────────────────────────┐
│                         lib.rs (library)                     │
│         Exports: InstanceFilterArgs, Architecture,          │
│                  SortField, all modules                      │
└─────────────────────────┬───────────────────────────────────┘
                          │
      ┌───────────────┬───┼───────────────┬──────────────┐
      ▼               ▼   ▼               ▼              ▼
┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
│ config.rs│   │  ssh.rs  │   │  aws/    │   │ utils.rs │
│          │   │          │   │          │   │          │
│ - load   │   │ - shell  │   │ - client │   │-tilde    │
│ - save   │   │   out to │   │ - pricing│   │  expand  │
│ - paths  │   │   ssh    │   │ - filters│   │-base64   │
└──────────┘   │ - wait   │   │-instances│   │-ssh opts │
               │   ready  │   └──────────┘   └──────────┘
┌──────────┐   └──────────┘
│  keys.rs │                  ┌──────────┐
│          │                  │ launchd/ │
│ - create │                  │ watch.rs │
│ - list   │                  │ -cleanup │
└──────────┘                  └──────────┘
```

## Module Responsibilities

### lib.rs

Library crate entry point. Exports all modules and shared types.

```rust
pub struct InstanceFilterArgs {
    pub instance_type: Option<String>,
    pub vcpu_min: Option<i32>,
    pub vcpu_max: Option<i32>,
    pub memory_min: Option<f64>,
    pub memory_max: Option<f64>,
    pub arch: Option<Architecture>,
    pub max_price: Option<f64>,
}
```

Design decisions:
- `InstanceFilterArgs` uses `clap::Args` derive for easy flattening into commands
- Shared filter struct eliminates duplication across `prices`, `start`, `ssh` commands
- Library can be used independently of CLI

### main.rs

Entry point and CLI definition using `clap` derive macros.

- Defines all commands and their arguments
- Uses `#[command(flatten)]` to embed `InstanceFilterArgs` in multiple commands
- Routes commands to appropriate handlers
- Handles async runtime setup with `tokio`

Design decisions:
- All commands are async to support AWS SDK operations
- Region can be overridden per-command or uses AWS SDK default
- JSON output option for scripting/automation
- `-t/--trace` flag at top level only (not global)

### config.rs

Configuration management using TOML files.

Location: `~/.config/spotty/config.toml`

```rust
pub struct Config {
    pub defaults: Defaults,  // region, key_name, key_path, ssh_user, security_group
    pub ami: HashMap<String, String>,  // region-arch -> AMI ID overrides
}
```

Design decisions:
- Uses `dirs` crate for cross-platform config paths
- Falls back to AWS SDK defaults when config missing (region from AWS_REGION or ~/.aws/config)
- Supports per-region AMI overrides for custom images

### keys.rs

SSH key pair management.

```rust
pub async fn create_key_pair(name: &str, region: Option<String>) -> Result<()>
pub async fn list_key_pairs(region: Option<String>) -> Result<()>
```

Design decisions:
- Creates Ed25519 keys (more secure, smaller)
- Saves private key to `~/.ssh/{name}.pem` with 0600 permissions
- Lists keys showing which have local private key files

### aws/client.rs

AWS SDK client initialization.

```rust
pub struct AwsClients {
    pub ec2: Ec2Client,
    pub ssm: SsmClient,
    pub region: String,
}
```

Design decisions:
- Single struct holds all AWS clients for a session
- Region resolved once at creation (CLI arg > config > AWS SDK default)
- Uses `BehaviorVersion::latest()` for SDK defaults

### aws/filters.rs

Instance type specification, filtering, and caching.

```rust
pub struct InstanceTypeSpec {
    pub instance_type: String,
    pub vcpu: i32,
    pub memory_mb: i64,
    pub architectures: Vec<String>,
}
```

Design decisions:
- Fetches all instance types via `DescribeInstanceTypes` API
- **Caches to disk** at `~/.cache/spotty/instance_types_{region}.json` with 24-hour TTL
- Filters applied client-side for flexibility

### aws/pricing.rs

Spot price queries and display.

```rust
pub struct SpotPrice {
    pub instance_type: String,
    pub vcpu: i32,
    pub memory_gb: String,
    pub arch: String,
    pub availability_zone: String,
    pub price: String,
    pub price_per_core: String,  // ¢/core/hr
}
```

Key operations:
1. Fetch instance type specs (from cache if available)
2. Query spot prices via shared `fetch_all_spot_prices` from instances.rs
3. **Both queries run concurrently** with `tokio::join!`
4. Join price data with instance specs
5. Apply filters and sort

Design decisions:
- Uses `Linux/UNIX` product description (most common)
- **Shares price fetching logic** with instances.rs via `fetch_all_spot_prices`
- Optimized single-type query when specific instance type requested
- Deduplicates to lowest price per instance-type + AZ
- Limits pagination to avoid very long waits

### aws/instances.rs

Instance lifecycle management.

Key types:
```rust
pub struct LaunchConfig {
    pub instance_type: Option<String>,
    pub vcpu_min: Option<i32>,
    pub vcpu_max: Option<i32>,
    pub memory_min: Option<f64>,
    pub memory_max: Option<f64>,
    pub arch: Option<String>,
    pub max_price: Option<f64>,
    pub ami: Option<String>,
    pub region: Option<String>,
    pub key_name: Option<String>,
    pub security_group: Option<String>,
    pub name: Option<String>,
    pub user_data: Option<String>,
    pub no_auto_cleanup: bool,
    pub max_tries: usize,
}
```

Key functions:
- `start_instances`: Launch spot instance(s) with `LaunchConfig`
- `start_instance_and_get_id`: Launch single instance, return ID (waits for running)
- `stop_instances`: Terminate instances (spot instances can't be stopped)
- `instance_status`: Query instance state with **cost tracking**
- `fetch_all_spot_prices`: Shared price fetching (used by pricing.rs too)
- `get_instance_public_ip`: Resolve IP for SSH
- `get_instance_key_name`: Get key name for SSH key lookup

Filter helpers (reduce duplication):
```rust
fn filter_managed_by_spotty() -> Filter
fn filter_running_or_pending() -> Filter
fn filter_active_states() -> Filter
fn filter_auto_cleanup_true() -> Filter
```

Design decisions:
- **`LaunchConfig` struct** replaces 16+ function parameters for cleaner API
- **Shared `launch_instances_impl`** eliminates duplication between start functions
- Auto-discovers AMI via SSM Parameter Store (`/aws/service/ami-amazon-linux-latest/...`)
- **Architecture determined from cached instance type data** (not guessed from name)
- Auto-discovers SSH key pair if not specified
- Creates default security group if none specified (SSH from 0.0.0.0/0)
- Tags instances with `ManagedBy=spotty` for filtering
- Supports waiting for instance to reach running state
- **Status shows**: Instance ID, Name, Type, Arch, vCPU, State, Public IP, Rate/hr, Cost, Running time
- **Status footer shows totals**: total vCPU, total rate, total cost, core-hours (running_secs × vcpu)
- **Minimum 1-minute billing**: Cost calculation uses `max(running_secs, 60)` for AWS billing accuracy

### ssh.rs

SSH connectivity via system binary.

```rust
pub async fn ssh_into_instance(
    instance_id: String,
    key: Option<String>,
    user: String,
    command: Option<String>,
    region: Option<String>,
) -> Result<()>
```

Design decisions:
- Shells out to system `ssh` binary (simpler, uses existing SSH agent/config)
- **Uses shared `add_ssh_options`** from utils.rs for host key checking options
- **Auto-discovers key file**: fetches instance's key name, looks for `~/.ssh/{name}.pem`
- **Uses shared `expand_tilde`** from utils.rs for path expansion
- Supports both interactive and command execution modes
- **Waits for SSH to be ready**: polls with `BatchMode=yes` until connection succeeds (max 30 attempts, 2s delay)

### output.rs

Output formatting utilities.

```rust
pub fn print_table<T: Tabled>(items: &[T])
pub fn print_json<T: Serialize>(items: &T) -> Result<()>
```

Design decisions:
- Uses `tabled` crate for ASCII tables
- JSON output for machine consumption / scripting
- Empty result handling with user-friendly message

### utils.rs

Shared utility functions to avoid code duplication across modules.

```rust
pub fn expand_tilde(path: &str) -> Cow<'_, str>
pub fn base64_encode(input: &str) -> String
pub fn add_ssh_options(cmd: &mut Command)
pub fn add_ssh_batch_options(cmd: &mut Command)
```

Design decisions:
- Centralizes tilde expansion
- Provides base64 encoding for user data (used by instances.rs)
- SSH options helpers reduce duplication of `-o StrictHostKeyChecking=no` patterns

### launchd.rs / watch.rs

Auto-cleanup watcher daemon for macOS.

```rust
pub fn install() -> Result<()>    // Install launchd agent
pub fn uninstall() -> Result<()>  // Remove launchd agent
pub fn status() -> Result<()>     // Show agent status
pub fn log() -> Result<()>        // Print watcher log
pub async fn setup() -> Result<()>  // Create IAM user with minimal permissions
pub async fn run_watch() -> Result<()>  // Run watcher in foreground
```

Design decisions:
- Uses macOS launchd for daemon management
- Monitors system sleep events via IOKit
- Terminates spotty-managed instances (tagged `ManagedBy=spotty`) on sleep
- Instances with `AutoCleanup=false` tag are excluded
- Auto-installs on first `spotty start` (unless `--no-auto-cleanup`)
- **Dedicated IAM user**: `spotty watch setup` creates `spotty-watcher` IAM user with minimal permissions:
  - `ec2:DescribeInstances` (all)
  - `ec2:TerminateInstances` (only resources tagged `ManagedBy=spotty`)
- Credentials stored in `~/.aws/credentials` under `[spotty-watcher]` profile
- Launchd plist sets `AWS_PROFILE=spotty-watcher` environment variable

## Data Flow

### Price Query Flow

```
User: spotty prices --vcpu-min 2 --arch arm64
                    │
                    ▼
            ┌───────────────┐
            │ Parse CLI args │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Create AWS    │
            │ clients       │
            └───────┬───────┘
                    │
        ┌───────────┴───────────┐
        ▼                       ▼
┌───────────────┐       ┌───────────────┐
│DescribeInst-  │       │DescribeSpot-  │
│anceTypes      │       │ PriceHistory  │
│ (cached)      │       │               │
└───────┬───────┘       └───────┬───────┘
        │  concurrent           │
        └───────────┬───────────┘
                    ▼
            ┌───────────────┐
            │ Join & Filter │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Sort & Limit  │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Output table  │
            │ or JSON       │
            └───────────────┘
```

### Instance Launch Flow

```
User: spotty start -t t3.medium --wait
                    │
                    ▼
            ┌───────────────┐
            │ Get instance  │◄─── From cache
            │ type arch     │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Resolve AMI   │◄─── SSM GetParameter
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Resolve Key   │◄─── DescribeKeyPairs (if needed)
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Resolve SG    │◄─── Create if needed
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ RunInstances  │
            │ (spot market) │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Wait for      │◄─── Poll DescribeInstances
            │ running state │
            └───────┬───────┘
                    │
                    ▼
            ┌───────────────┐
            │ Output status │
            └───────────────┘
```

## Error Handling

- Uses `anyhow` for error propagation with context
- `thiserror` available for structured errors if needed
- Errors bubble up to main and display to user
- AWS SDK errors wrapped with context about the operation

## Security Considerations

1. **Credentials**: Never stored by spotty, uses standard AWS SDK chain
2. **SSH Keys**: Created with 0600 permissions; spotty manages key lifecycle
3. **Security Groups**: Default SG allows SSH from anywhere (0.0.0.0/0) - user should restrict
4. **Host Key Checking**: Disabled for convenience; acceptable for ephemeral instances

## Testing Strategy

1. **Unit tests**: Filter logic, output formatting, config parsing
2. **Integration tests**: Require AWS credentials, create real resources
3. **Manual testing**: Full workflows with actual instances

## Performance Considerations

- **Instance type data cached to disk** with 24-hour TTL to reduce API calls
- **Concurrent API calls** for prices command (instance types + spot prices in parallel)
- Spot price queries are paginated with reasonable limits
- SSH operations are synchronous (blocking on external process)
