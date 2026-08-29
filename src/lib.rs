//! Spotty - AWS compute management library
//!
//! Provides APIs for running workloads on AWS using EC2 spot instances
//! and Lambda functions. Supports querying prices, launching and
//! terminating instances, SSH access, file deployment, AMI image
//! management, and Lambda function lifecycle.
//!
//! # Modules
//!
//! - [`aws`] - AWS API operations:
//!   - [`aws::instances`] - EC2 spot instance lifecycle
//!   - [`aws::pricing`] - Spot price queries
//!   - [`aws::images`] - AMI image management
//!   - [`aws::lambda`] - Lambda function deployment and invocation
//!   - [`aws::client`] - AWS client setup
//! - [`config`] - Configuration file management (`~/.config/spotty/config.toml`)
//! - [`keys`] - SSH key pair creation and listing
//! - [`ssh`] - SSH connections and file deployment via SCP
//! - [`state`] - Local instance tracking (`~/.cache/spotty/instances.json`)
//! - [`output`] - Table and JSON output formatting
//! - [`utils`] - Shared utilities (base64, tilde expansion, SSH options)
//!
//! On macOS, additional modules are available:
//! - `launchd` - launchd agent management for the auto-cleanup watcher
//! - `watch` - Sleep/wake detection for automatic instance cleanup

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod ec2 {
    include!(concat!(env!("OUT_DIR"), "/ec2.rs"));

    impl From<&str> for InstanceType {
        fn from(s: &str) -> Self {
            Self::from_str(s)
        }
    }
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod ssm {
    include!(concat!(env!("OUT_DIR"), "/ssm.rs"));
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod iam {
    include!(concat!(env!("OUT_DIR"), "/iam.rs"));
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod lambda_api {
    include!(concat!(env!("OUT_DIR"), "/lambda.rs"));
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod sts {
    include!(concat!(env!("OUT_DIR"), "/sts.rs"));
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod sso_oidc {
    include!(concat!(env!("OUT_DIR"), "/sso-oidc.rs"));
}

#[allow(dead_code, non_snake_case, unused_variables, clippy::all)]
pub mod sso_portal {
    include!(concat!(env!("OUT_DIR"), "/sso.rs"));
}

pub mod aws;
pub mod config;
pub mod keys;
#[cfg(target_os = "macos")]
pub mod launchd;
pub mod output;
pub mod setup;
pub mod ssh;
pub mod state;
pub mod utils;
#[cfg(target_os = "macos")]
pub mod watch;

use clap::Args;

/// Common filter options for instance type selection
#[derive(Args, Clone, Debug, Default)]
pub struct InstanceFilterArgs {
    /// Instance type (e.g., m5.large)
    #[arg(long)]
    pub instance_type: Option<String>,

    /// Minimum vCPU count
    #[arg(long)]
    pub vcpu_min: Option<i32>,

    /// Maximum vCPU count
    #[arg(long)]
    pub vcpu_max: Option<i32>,

    /// Minimum memory in GB
    #[arg(long)]
    pub memory_min: Option<f64>,

    /// Maximum memory in GB
    #[arg(long)]
    pub memory_max: Option<f64>,

    /// Architecture: x86_64, arm64
    #[arg(long, short)]
    pub arch: Option<Architecture>,

    /// Maximum price per hour in USD
    #[arg(long)]
    pub max_price: Option<f64>,
}

impl InstanceFilterArgs {
    /// Returns the architecture as a string slice
    pub fn arch_str(&self) -> Option<&str> {
        self.arch.as_ref().map(|a| a.as_str())
    }

    /// Check if any filter is specified
    pub fn has_any_filter(&self) -> bool {
        self.instance_type.is_some()
            || self.vcpu_min.is_some()
            || self.vcpu_max.is_some()
            || self.memory_min.is_some()
            || self.memory_max.is_some()
            || self.arch.is_some()
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Architecture {
    X86_64,
    Arm64,
}

impl Architecture {
    pub fn as_str(&self) -> &'static str {
        match self {
            Architecture::X86_64 => "x86_64",
            Architecture::Arm64 => "arm64",
        }
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, Default)]
pub enum SortField {
    #[default]
    Price,
    Vcpu,
    Memory,
}
