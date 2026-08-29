# Spotty

A command-line tool for running workloads on AWS. Supports EC2 spot instances (long-running, SSH access) and Lambda functions (up to 15 min, serverless). Deploy custom binaries, build AMIs, and manage compute from your terminal.

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
./target/release/spotty --help
```

## Setup

```bash
spotty setup
```

This logs you in to IAM Identity Center and prepares the account. It is the
only command that needs administrative access, and it uses that access exactly
once — to create a scoped IAM role that everything afterwards runs as.

```
$ spotty setup
IAM Identity Center
  start URL : https://d-xxxxxxxxxx.awsapps.com/start/
  session   : my-sso

Confirm this login in a browser:

  https://d-xxxxxxxxxx.awsapps.com/start/#/device?user_code=GQJD-SHHP

Waiting for approval...
```

Useful flags:

| Flag | Effect |
| --- | --- |
| `--status` | Show the current login, credential source and identity |
| `--check` | Verify the credentials carry the permissions spotty needs; exits non-zero if any are missing |
| `--dry-run` | Log in and print the policies, changing nothing |
| `--no-browser` | Print the link instead of opening a browser |
| `--export` | Print shell exports for the scoped credentials |
| `--logout` | Forget cached role credentials |

### Why a scoped role

An SSO permission set usually grants far more than spotty needs —
`AdministratorAccess`, in a lot of setups. Rather than run every command with
that authority, `spotty setup` creates an IAM role named `spotty` whose policy
covers only what the tool actually calls, and day-to-day commands assume it:

```
SSO login  →  permission set  →  sts:AssumeRole  →  spotty role  →  EC2/Lambda
             (setup only)                          (1h, cached)
```

The permission-set credentials are used for the `AssumeRole` call and are never
written to disk. Assumed-role credentials last an hour and are cached in
`~/.cache/spotty/credentials.json` (mode 0600).

The role's policy is narrower than a flat allow-list in two ways worth knowing:
terminating instances, imaging them and deregistering AMIs are conditioned on
the `ManagedBy=spotty` tag, so the role cannot touch resources it did not
create; and Lambda permissions are restricted to functions named `spotty-*`.
Run `spotty setup --dry-run` to see the exact policy before it is applied.

Tightening `DeregisterImage`/`DeleteSnapshot` onto that tag means AMIs created
by a spotty older than this one — whose backing snapshots were left untagged —
have to be removed with a credential outside the scoped role.

`spotty setup --status` will tell you if you are still running with
permission-set privileges rather than the scoped role.

### Credential resolution order

1. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` — an explicit override always wins
2. Cached scoped-role credentials, while still valid
3. Cached SSO token → permission set → `sts:AssumeRole` into the `spotty` role
4. `aws configure export-credentials`, for setups that do not use SSO

### Logging in over SSH

The login uses the OAuth 2.0 device authorization grant, which is designed for
machines without a browser. On a remote host, `spotty setup` prints a link and
a short code; you approve it on any other device. Nothing secret is copied
between machines — the user code is a public one-time code — and no port
forwarding is needed.

Spotty skips the browser automatically when it detects an SSH session or a
machine with no display. Force it with `--no-browser`.

For CI, or a host that should not enrol in SSO at all, mint scoped credentials
elsewhere and pass them in:

```bash
spotty setup --export        # prints AWS_* exports, valid for one hour
```

These are real secrets, unlike the device code — but they are short-lived and
carry only the scoped role's permissions. For instances spotty launches itself,
prefer an EC2 instance profile so no credentials travel at all.

### Without IAM Identity Center

Spotty also accepts ordinary credentials, in which case it runs with whatever
those grant and no role is created:

```bash
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
export AWS_REGION=us-east-1
```

### Required IAM Permissions

`spotty setup` attaches these to the `spotty` role for you. You need them (plus
`iam:CreateRole`, `iam:PutRolePolicy` and `sts:AssumeRole`) on the identity you
run `setup` with:

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
        "ssm:GetParameter",
        "lambda:CreateFunction",
        "lambda:DeleteFunction",
        "lambda:GetFunction",
        "lambda:GetFunctionConfiguration",
        "lambda:InvokeFunction",
        "lambda:ListFunctions",
        "lambda:UpdateFunctionCode",
        "lambda:UpdateFunctionConfiguration",
        "lambda:TagResource",
        "lambda:ListTags",
        "iam:CreateRole",
        "iam:GetRole",
        "iam:AttachRolePolicy",
        "iam:PassRole",
        "sts:GetCallerIdentity"
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

### 1. Log in and prepare the account

```bash
spotty setup
```

Besides logging in and creating the scoped role, this creates an SSH key pair
and a `spotty-default` security group if you do not already have them, and
writes the result to `~/.config/spotty/config.toml`:

```toml
[defaults]
region = "eu-north-1"
role_arn = "arn:aws:iam::123456789012:role/spotty"   # written by `spotty setup`
ssh_user = "ec2-user"

[regions.eu-north-1]
key_name = "spotty-key-tink"     # Name of your EC2 key pair
key_path = "~/.ssh/spotty-key-tink.pem"
security_group = "sg-0123456789abcdef0"
```

Key pairs and security groups exist inside a single region, so `setup` records
them under `[regions.<name>]`. Running `spotty setup --region <other>` adds a
second block rather than overwriting the first. Values you set under
`[defaults]` fill in whatever a region block leaves out, but only in the region
`defaults.region` names — a `sg-...` from one region is meaningless in another.

### Using spotty from more than one machine

AWS hands out a key pair's private key exactly once, when it is created, so a
key pair made on one machine cannot be used from another. `spotty setup` names
its key pair after the host — `spotty-key-tink` — so each machine creates and
owns its own, and running setup on a second machine no longer trips over the
first machine's key.

The consequence is that an instance is reachable from the machine that launched
it: the key pair is baked into the instance at launch. If you need to launch on
one machine and SSH from another, share one key pair instead — import a public
key you already have on both:

```bash
aws ec2 import-key-pair --key-name spotty-shared \
    --public-key-material fileb://~/.ssh/id_ed25519.pub --region eu-north-1
spotty config set key_name spotty-shared
spotty config set key_path ~/.ssh/id_ed25519
```

Pass `--skip-ec2` to leave the key pair and security group alone, or manage
them yourself:

```bash
spotty keys create my-key
# Creates key in AWS and saves to ~/.ssh/my-key.pem
```

`spotty config init` still writes a bare config file if you would rather set
everything by hand.

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

## Lambda Functions

Lambda provides a serverless compute backend for short jobs (up to 15 minutes). Deploy a compiled binary and invoke it without managing instances.

### Deploy a Binary

```bash
# Cross-compile for Lambda (Amazon Linux 2023)
cargo build --release --target x86_64-unknown-linux-gnu

# Deploy as a Lambda function (auto-named spotty-<binary>)
spotty lambda deploy ./target/x86_64-unknown-linux-gnu/release/myapp

# With custom settings
spotty lambda deploy ./myapp --name my-service --memory 256 --timeout 300 --arch arm64
```

The binary is packaged as `bootstrap` in a zip and deployed with the `provided.al2023` custom runtime. An IAM execution role (`spotty-lambda-execution`) is auto-created on first deploy.

### Start (Invoke)

```bash
# Invoke and wait for result
spotty lambda start my-service

# With JSON payload
spotty lambda start my-service --payload '{"input": "data"}'

# Fire-and-forget
spotty lambda start my-service --async
```

### List and Status

```bash
# List all spotty-managed functions
spotty lambda list

# Show details for a specific function
spotty lambda status my-service
```

### Stop (Delete)

```bash
# Delete a function
spotty lambda stop my-service

# Delete all spotty-managed functions
spotty lambda stop --all
```

### EC2 vs Lambda

| | EC2 Spot | Lambda |
|---|---|---|
| Duration | Unlimited | Up to 15 min |
| Access | SSH, SCP | API invoke only |
| Binary delivery | `spotty deploy` (SCP) | `spotty lambda deploy` (zip) |
| Start | `spotty start` | `spotty lambda start` |
| Custom images | AMI (`spotty image build`) | Container (planned) |
| Cost | Per-hour while running | Per-invocation |
| Cleanup needed | Yes | No (no idle cost) |

## Configuration Reference

Config file location: `~/.config/spotty/config.toml`

```toml
[defaults]
# region = "us-east-1"         # Optional: uses AWS SDK default (AWS_REGION, ~/.aws/config)
ssh_user = "ec2-user"          # Default SSH username

# Written by `spotty setup`, one block per region it has run in.
# Values under [defaults] are used only in the region named there.
[regions.us-east-1]
# key_name = "my-keypair"        # Optional: auto-discovered from available keys
# key_path = "~/.ssh/my-key.pem" # Optional: auto-discovered from key_name
# security_group = "sg-12345"    # Optional: auto-created if not set

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

### "not logged in" / "no AWS credentials found"

Check what spotty is resolving and whether the SSO token is still valid:
```bash
spotty setup --status
```

Log in again with:
```bash
spotty setup
```

### "failed to assume arn:aws:iam::...:role/spotty"

The scoped role is missing or its trust policy changed. Re-create it:
```bash
spotty setup
```

If commands fail with `UnauthorizedOperation` after the role exists, its policy
may be stale — `spotty setup --check` reports which permissions are missing,
and re-running `spotty setup` refreshes the policy.

### The watcher's IAM user

`spotty watch setup` deliberately uses long-lived access keys rather than the
scoped role: it has to terminate instances after your SSO session has expired,
with nobody present to log in again. Its policy is limited to
`ec2:DescribeInstances` and terminating instances tagged `ManagedBy=spotty`.

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
