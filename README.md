# Spotty

A command-line tool for managing AWS EC2 spot instances. Query spot prices, launch instances, and SSH in - all from your terminal.

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
        "ssm:GetParameter"
      ],
      "Resource": "*"
    }
  ]
}
```

For `spotty watch setup`, you also need IAM permissions (one-time setup):

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
# List cheapest instances (shows price/hr and ¢/core/hr)
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

### 6. Terminate Instances

```bash
# Single instance
spotty stop i-0123456789abcdef0

# Multiple instances
spotty stop i-abc123 i-def456 i-ghi789

# Terminate all spotty-managed instances
spotty stop --all
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

# Or use AWS CLI directly
aws ec2 create-key-pair --key-name spotty-key --query 'KeyMaterial' --output text > ~/.ssh/spotty-key.pem
chmod 400 ~/.ssh/spotty-key.pem
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

## Auto-Cleanup Watcher

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

This ensures the watcher daemon can only terminate spotty-managed instances, even if credentials are compromised.

The setup command:
1. Creates IAM user `spotty-watcher`
2. Attaches a minimal permissions policy
3. Creates access keys
4. Stores credentials in `~/.aws/credentials` under `[spotty-watcher]` profile
5. Configures the launchd agent to use this profile

Use `--no-auto-cleanup` when starting instances to exclude them from auto-cleanup:
```bash
spotty start --instance-type t3.micro --no-auto-cleanup
```

## Debugging

Enable verbose tracing output with `-t` or `--trace`:

```bash
spotty -t prices --vcpu-min 2
spotty -t start --instance-type t3.micro
```

## License

MIT
