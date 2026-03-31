use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::client::AwsClients;
use crate::ec2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceTypeSpec {
    pub instance_type: String,
    pub vcpu: i32,
    pub memory_mb: i64,
    pub architectures: Vec<String>,
}

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

fn cache_path(region: &str) -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spotty")
        .join(format!("instance_types_{}.json", region))
}

fn load_cached_instance_types(region: &str) -> Option<HashMap<String, InstanceTypeSpec>> {
    let path = cache_path(region);
    let metadata = fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;

    if SystemTime::now().duration_since(modified).ok()? > CACHE_TTL {
        return None;
    }

    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_instance_types_cache(region: &str, types: &HashMap<String, InstanceTypeSpec>) {
    let path = cache_path(region);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(types) {
        let _ = fs::write(&path, content);
    }
}

impl InstanceTypeSpec {
    pub fn memory_gb(&self) -> f64 {
        self.memory_mb as f64 / 1024.0
    }
}

pub struct InstanceTypeFilter {
    pub vcpu_min: Option<i32>,
    pub vcpu_max: Option<i32>,
    pub memory_min_gb: Option<f64>,
    pub memory_max_gb: Option<f64>,
    pub arch: Option<String>,
}

impl InstanceTypeFilter {
    pub fn matches(&self, spec: &InstanceTypeSpec) -> bool {
        if let Some(min) = self.vcpu_min {
            if spec.vcpu < min {
                return false;
            }
        }
        if let Some(max) = self.vcpu_max {
            if spec.vcpu > max {
                return false;
            }
        }
        if let Some(min) = self.memory_min_gb {
            if spec.memory_gb() < min {
                return false;
            }
        }
        if let Some(max) = self.memory_max_gb {
            if spec.memory_gb() > max {
                return false;
            }
        }
        if let Some(ref arch) = self.arch {
            if !spec.architectures.iter().any(|a| a == arch) {
                return false;
            }
        }
        true
    }
}

pub async fn get_instance_types(
    clients: &AwsClients,
) -> Result<HashMap<String, InstanceTypeSpec>> {
    // Try cache first
    if let Some(cached) = load_cached_instance_types(&clients.region) {
        return Ok(cached);
    }

    let mut instance_types = HashMap::new();
    let mut next_token: Option<String> = None;

    loop {
        let mut req = ec2::DescribeInstanceTypesRequest::default();
        if let Some(token) = next_token {
            req = req.next_token(token);
        }

        let resp = ec2::describe_instance_types(&clients.client, req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        for info in resp.instance_types.iter().flat_map(|l| &l.item) {
            if let Some(spec) = parse_instance_type_info(info) {
                instance_types.insert(spec.instance_type.clone(), spec);
            }
        }

        next_token = resp.next_token.clone();
        if next_token.is_none() {
            break;
        }
    }

    // Save to cache
    save_instance_types_cache(&clients.region, &instance_types);

    Ok(instance_types)
}

fn parse_instance_type_info(info: &ec2::InstanceTypeInfo) -> Option<InstanceTypeSpec> {
    let instance_type = info.instance_type.as_ref()?.to_string();

    let vcpu = info.v_cpu_info.as_ref()?.default_v_cpus?;

    let memory_mb = info.memory_info.as_ref()?.size_in_mi_b?;

    let architectures = info
        .processor_info
        .as_ref()?
        .supported_architectures
        .as_ref()?
        .item
        .iter()
        .map(|a| a.to_string())
        .collect();

    Some(InstanceTypeSpec {
        instance_type,
        vcpu,
        memory_mb,
        architectures,
    })
}
