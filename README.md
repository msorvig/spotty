# Spotty

A command-line tool for managing AWS EC2 spot instances. Query spot prices, launch instances, deploy files, build custom AMIs, and SSH in - all from your terminal.

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
./target/release/spotty --help
```

## AWS Credentials Setup

Spotty uses the standard AWS SDK credential chain. Configure credentials using one of these methods:

### Option 1: AWS SSO

```bash
aws sso login --profile my-profile
export AWS_PROFILE=my-profile
```

### Option 2: Environment Variables

```bash
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
export AWS_REGION=us-east-1
```

### Required IAM Permissions

Your IAM user/role needs these permissions:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ec2:DescribeSpotPriceHistory",
        "ec2:DescribeInstanceTypes",
        "ec2:DescribeInstances",
        "ec2:RunInstances",
        "ec2:TerminateInstances",
        "ec2:CreateTags",
        "ec2:DescribeSecurityGroups",
        "ec2:CreateSecurityGroup",
        "ec2:AuthorizeSecurityGroupIngress",
        "ec2:DescribeVpcs",
        "ec2:DescribeKeyPairs",
        "ec2:CreateKeyPair",
        "ec2:CreateImage",
        "ec2:DeregisterImage",
        "ec2:DescribeImages",
        "ec2:DescribeSnapshots",
        "ec2:DeleteSnapshot",
        "ssm:GetParameter"
      ],
      "Resource": "*"
    }
  ]
}
```

For `spotty watch setup` (macOS only), you also need IAM permissions (one-time setup):

```json
{
  "Effect": "Allow",
  "Action": [
    "iam:CreateUser",
    "iam:GetUser",
    "iam:PutUserPolicy",
    "iam:CreateAccessKey",
    "iam:DeleteAccessKey",
    "iam:ListAccessKeys"
  ],
  "Resource": "arn:aws:iam::*:user/spotty-watcher"
}
```

## Quick Start

### 1. Initialize Configuration

```bash
spotty config init
```

Edit `~/.config/spotty/config.toml`:

```toml
[defaults]
# region = "us-east-1"          # Optional: uses AWS_REGION or ~/.aws/config by default
key_name = "my-aws-keypair"      # Name of your EC2 key pair (auto-discovered if not set)
key_path = "~/.ssh/my-key.pem"   # Path to private key file (auto-discovered from key_name)
ssh_user = "ec2-user"
```

Or create a key pair with spotty:

```bash
spotty keys create my-key
# Creates key in AWS and saves to ~/.ssh/my-key.pem
```

### 2. Find Cheap Spot Instances

```bash
# List cheapest instances (shows price/hr and c/core/hr)
spotty prices --limit 10

# Filter by specs
spotty prices --vcpu-min 2 --vcpu-max 4 --memory-min 4 --memory-max 16

# ARM instances only
spotty prices --arch arm64 --max-price 0.05

# Specific instance type
spotty prices -t m5.large
```

### 3. Launch an Instance

```bash
# Basic launch (auto-discovers key pair and AMI)
spotty start --instance-type t3.micro

# Launch with filters - tries cheapest matching instance until one starts
spotty start --vcpu-min 2 --memory-min 4 --arch arm64

# Wait for it to be ready
spotty start --instance-type t3.medium --wait --name "my-server"

# Launch multiple instances
spotty start --vcpu-min 4 --number 3

# With specific key and custom AMI
spotty start --instance-type t3.large --ami ami-0123456789abcdef0 --key-name my-key

# Custom root volume size
spotty start --instance-type t3.medium --volume-size 50
```

### 4. Check Status

```bash
# Show all spotty-managed instances (includes cost info)
spotty status
# Shows: Instance ID, Name, Type, Arch, vCPU, State, Public IP, Rate/hr, Cost, Running time
# Footer shows totals: total vCPU, total rate, total cost, core-hours

# Show specific instance
spotty status i-0123456789abcdef0

# Include terminated instances
spotty status --all

# JSON output
spotty status --json
```

### 5. SSH Into Instance

```bash
# Interactive shell (auto-waits for SSH to be ready)
spotty ssh i-0123456789abcdef0

# Run a command
spotty ssh i-0123456789abcdef0 -c "uname -a"

# With specific key
spotty ssh i-0123456789abcdef0 --key ~/.ssh/other-key.pem

# Start a new instance and SSH in (uses filter args)
spotty ssh --vcpu-min 2 --memory-min 4 --arch arm64
```

### 6. Deploy Files

Upload local files to a running instance via SCP:

```bash
# Deploy a binary (auto-selects instance if only one running)
spotty deploy ./target/release/myapp

# Deploy to a specific directory
spotty deploy ./myapp --to /usr/local/bin

# Deploy and run a command after
spotty deploy ./myapp --run "chmod +x ~/myapp && ~/myapp --version"

# Deploy multiple files
spotty deploy ./myapp ./config.toml --to ~/app

# Deploy to a specific instance
spotty deploy ./myapp --instance i-0123456789abcdef0
```

### 7. Terminate Instances

```bash
# Single instance
spotty stop i-0123456789abcdef0

# Multiple instances
spotty stop i-abc123 i-def456 i-ghi789

# Terminate all spotty-managed instances
spotty stop --all
```

## Custom AMI Images

### List Available AMIs

```bash
# Shows base AL2023 AMIs and any custom AMIs from config
spotty image list
```

### Build a Custom AMI from a Script

Launch an instance, run a build script on it, and bake the result into an AMI:

```bash
# Build with a script (launches instance, runs script, bakes AMI, terminates)
spotty image build --script setup.sh --arch arm64

# With a custom name and volume size
spotty image build --script setup.sh --name "my-app-v1" --volume-size 40
```

### Build a Custom AMI with a Deployed Binary

Cross-compile locally, deploy to an instance, and bake:

```bash
# 1. Launch an instance
spotty start --arch x86_64 --wait

# 2. Cross-compile your Rust binary
cargo build --release --target x86_64-unknown-linux-gnu

# 3. Deploy the binary and any config files
spotty deploy ./target/x86_64-unknown-linux-gnu/release/myapp ./config.toml \
  --to ~/app --run "chmod +x ~/app/myapp"

# 4. Bake the instance into an AMI
spotty image create --wait --name "myapp-v1"

# 5. Terminate the builder instance
spotty stop --all
```

### Create an AMI from a Running Instance

```bash
# Bake current state into an AMI
spotty image create --wait

# With a custom name
spotty image create i-0123456789abcdef0 --name "my-snapshot" --wait
```

### Remove an AMI

```bash
# Deregister AMI and delete backing snapshots
spotty image rm ami-0123456789abcdef0
```

### Launch from a Custom AMI

```bash
# Use a specific AMI
spotty start --ami ami-0123456789abcdef0

# Or configure a default in config.toml:
spotty config set ami.us-east-1-x86_64 ami-0123456789abcdef0
```

## Configuration Reference

Config file location: `~/.config/spotty/config.toml`

```toml
[defaults]
# region = "us-east-1"         # Optional: uses AWS SDK default (AWS_REGION, ~/.aws/config)
# key_name = "my-keypair"      # Optional: auto-discovered from available keys
# key_path = "~/.ssh/my-key.pem" # Optional: auto-discovered from key_name
ssh_user = "ec2-user"          # Default SSH username
# security_group = "sg-12345"  # Optional: auto-created if not set

[ami]
# Override AMI discovery per region/architecture
us-east-1-x86_64 = "ami-0123456789abcdef0"
us-east-1-arm64 = "ami-0fedcba9876543210"
```

### Set Individual Values

```bash
spotty config set region us-west-2
spotty config set key_name my-new-key
spotty config set key_path ~/.ssh/new-key.pem
```

## Tips

### Managing SSH Key Pairs

```bash
# Create a new key pair (recommended)
spotty keys create my-key
# Creates key in AWS and saves private key to ~/.ssh/my-key.pem

# List available key pairs
spotty keys list
# Shows: NAME, TYPE, LOCAL FILE (whether private key exists locally)
```

### Finding the Right Instance Type

```bash
# Good for development (cheap, small)
spotty prices --vcpu-max 2 --memory-max 4 --arch x86-64 --limit 5

# Good for builds (more CPU)
spotty prices --vcpu-min 4 --vcpu-max 8 --memory-min 8 --limit 5

# Good for ML/GPU workloads - check g4dn, p3, etc.
spotty prices -t g4dn.xlarge

# ARM instances are often cheaper
spotty prices --arch arm64 --vcpu-min 2 --limit 10
```

### Spot Instance Best Practices

1. **Set max price**: Use `--max-price` to avoid unexpected costs if spot prices spike
2. **Use multiple AZs**: Spot availability varies by availability zone
3. **Save your work**: Spot instances can be terminated with 2-minute warning
4. **Use ARM when possible**: Graviton instances are typically 20-40% cheaper

### SSH Config Integration

Add to `~/.ssh/config` for easier access:

```
Host spotty-*
    User ec2-user
    IdentityFile ~/.ssh/spotty-key.pem
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
```

## Auto-Cleanup Watcher (macOS)

Spotty includes a watcher daemon that automatically terminates instances when your Mac goes to sleep (to avoid forgotten instances accumulating costs).

```bash
# Set up IAM user with minimal permissions (run once)
spotty watch setup

# Install the launchd agent (auto-starts on login)
spotty watch install

# Check watcher status
spotty watch status

# View the watcher log
spotty watch log

# Uninstall the agent
spotty watch uninstall

# Run watcher in foreground (for debugging)
spotty watch
```

### Watcher Setup

The `spotty watch setup` command creates a dedicated IAM user (`spotty-watcher`) with minimal permissions:
- `ec2:DescribeInstances` - to find running instances
- `ec2:TerminateInstances` - only for instances tagged with `ManagedBy=spotty`

Use `--no-auto-cleanup` when starting instances to exclude them from auto-cleanup:
```bash
spotty start --instance-type t3.micro --no-auto-cleanup
```

## Troubleshooting

### "No credentials found"

Ensure AWS credentials are configured:
```bash
aws sts get-caller-identity
```

### "Instance has no public IP"

The instance may be in a private subnet or still starting. Check status:
```bash
spotty status i-xxxxx
```

### "SSH connection refused"

Wait for the instance to fully boot (use `--wait` flag), or check security group allows SSH (port 22).

## Debugging

Enable verbose tracing output with `-t` or `--trace`:

```bash
spotty -t prices --vcpu-min 2
spotty -t start --instance-type t3.micro
```

## License

CC0 1.0 Universal
