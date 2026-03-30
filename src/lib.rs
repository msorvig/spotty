//! Spotty - AWS EC2 Spot Instance library
//!
//! This library provides APIs for managing AWS EC2 spot instances.

pub mod aws;
pub mod config;
pub mod keys;
pub mod launchd;
pub mod output;
pub mod ssh;
pub mod state;
pub mod utils;
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
