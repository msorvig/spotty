use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    /// Per-region settings, keyed by region name. Security groups and key
    /// pairs exist inside one region only, so `spotty setup` records what it
    /// created here rather than in `[defaults]`.
    #[serde(default)]
    pub regions: std::collections::HashMap<String, RegionDefaults>,
    #[serde(default)]
    pub ami: std::collections::HashMap<String, String>,
}

/// Settings that are only meaningful within a single region.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RegionDefaults {
    pub key_name: Option<String>,
    pub key_path: Option<String>,
    pub security_group: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Defaults {
    pub region: Option<String>,
    pub key_name: Option<String>,
    pub key_path: Option<String>,
    pub ssh_user: Option<String>,
    pub security_group: Option<String>,
    /// Scoped IAM role that day-to-day commands assume, written by
    /// `spotty setup`. Without it, commands fall back to running with whatever
    /// the SSO permission set grants.
    pub role_arn: Option<String>,
    /// Profile in `~/.aws/config` to read SSO settings from. Defaults to
    /// `$AWS_PROFILE`, then `default`.
    pub sso_profile: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = path();
        if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config from {:?}", path))?;
            toml::from_str(&content).with_context(|| "Failed to parse config file")
        } else {
            Ok(Config::default())
        }
    }

    /// Settings for `region`, falling back to `[defaults]` only when those
    /// were recorded for this same region.
    ///
    /// A security-group id or key-pair name from one region is never valid in
    /// another: without the region check, `spotty start --region <other>` would
    /// pass a foreign `sg-...` to EC2 and fail with InvalidGroup.NotFound
    /// instead of creating the group it needs.
    pub fn region_defaults(&self, region: &str) -> RegionDefaults {
        let entry = self.regions.get(region);

        // Merge field by field rather than letting a `[regions]` block shadow
        // `[defaults]` wholesale: setup writes a partial block (a security
        // group but no key pair, say), and the fields it left out should still
        // fall through to whatever the user set by hand.
        let fallback = match self.defaults.region.as_deref() {
            Some(r) if r != region => &Defaults::default(),
            _ => &self.defaults,
        };

        let pick = |from_region: Option<&String>, from_defaults: &Option<String>| {
            from_region.cloned().or_else(|| from_defaults.clone())
        };

        RegionDefaults {
            key_name: pick(entry.and_then(|e| e.key_name.as_ref()), &fallback.key_name),
            key_path: pick(entry.and_then(|e| e.key_path.as_ref()), &fallback.key_path),
            security_group: pick(
                entry.and_then(|e| e.security_group.as_ref()),
                &fallback.security_group,
            ),
        }
    }

    /// The per-region entry for `region`, created empty if absent.
    pub fn region_defaults_mut(&mut self, region: &str) -> &mut RegionDefaults {
        self.regions.entry(region.to_string()).or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }
}

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spotty")
        .join("config.toml")
}

pub fn show_config() -> Result<()> {
    let _config = Config::load()?;
    let path = path();

    println!("Config file: {:?}", path);
    println!();

    if path.exists() {
        let content = fs::read_to_string(&path)?;
        println!("{}", content);
    } else {
        println!("No config file found. Run 'spotty config init' to create one.");
    }

    Ok(())
}

pub fn set_config(key: &str, value: &str) -> Result<()> {
    let mut config = Config::load()?;

    match key {
        "region" => config.defaults.region = Some(value.to_string()),
        "key_name" => config.defaults.key_name = Some(value.to_string()),
        "key_path" => config.defaults.key_path = Some(value.to_string()),
        "ssh_user" => config.defaults.ssh_user = Some(value.to_string()),
        "security_group" => config.defaults.security_group = Some(value.to_string()),
        "role_arn" => config.defaults.role_arn = Some(value.to_string()),
        "sso_profile" => config.defaults.sso_profile = Some(value.to_string()),
        _ => {
            if key.starts_with("ami.") {
                let ami_key = key.strip_prefix("ami.").unwrap();
                config.ami.insert(ami_key.to_string(), value.to_string());
            } else {
                anyhow::bail!("Unknown config key: {}", key);
            }
        }
    }

    config.save()?;
    println!("Set {} = {}", key, value);

    Ok(())
}

pub fn init_config() -> Result<()> {
    let path = path();

    if path.exists() {
        println!("Config file already exists at {:?}", path);
        return Ok(());
    }

    let config = Config {
        defaults: Defaults {
            region: None, // Uses AWS SDK default (AWS_REGION, ~/.aws/config)
            key_name: None,
            key_path: None,
            ssh_user: Some("ec2-user".to_string()),
            security_group: None,
            role_arn: None,
            sso_profile: None,
        },
        regions: std::collections::HashMap::new(),
        ami: std::collections::HashMap::new(),
    };

    config.save()?;
    println!("Created config file at {:?}", path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_defaults(region: Option<&str>) -> Config {
        Config {
            defaults: Defaults {
                region: region.map(String::from),
                security_group: Some("sg-home".into()),
                key_name: Some("spotty-key".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn per_region_entry_wins() {
        let mut config = config_with_defaults(Some("eu-north-1"));
        config.regions.insert(
            "us-east-1".into(),
            RegionDefaults { security_group: Some("sg-east".into()), ..Default::default() },
        );
        assert_eq!(config.region_defaults("us-east-1").security_group.as_deref(), Some("sg-east"));
        assert_eq!(config.region_defaults("eu-north-1").security_group.as_deref(), Some("sg-home"));
    }

    #[test]
    fn legacy_defaults_do_not_leak_across_regions() {
        // A security group written by an older `spotty setup` belongs to the
        // region that setup ran in; reusing it elsewhere fails at EC2 with
        // InvalidGroup.NotFound instead of creating the group that is needed.
        let config = config_with_defaults(Some("eu-north-1"));
        assert_eq!(config.region_defaults("eu-north-1").security_group.as_deref(), Some("sg-home"));

        let elsewhere = config.region_defaults("us-east-1");
        assert_eq!(elsewhere.security_group, None);
        assert_eq!(elsewhere.key_name, None);
    }

    #[test]
    fn a_partial_region_block_does_not_hide_defaults() {
        // `setup` writes whatever it managed to create — a security group but
        // no key pair, if the key step was skipped. The fields it left out
        // still have to fall through to what the user set by hand, or
        // `spotty config set key_name ...` would silently do nothing.
        let mut config = config_with_defaults(Some("eu-north-1"));
        config.regions.insert(
            "eu-north-1".into(),
            RegionDefaults { security_group: Some("sg-fromsetup".into()), ..Default::default() },
        );

        let merged = config.region_defaults("eu-north-1");
        assert_eq!(merged.security_group.as_deref(), Some("sg-fromsetup"));
        assert_eq!(merged.key_name.as_deref(), Some("spotty-key"));
    }

    #[test]
    fn defaults_apply_everywhere_when_no_region_is_pinned() {
        let config = config_with_defaults(None);
        assert_eq!(config.region_defaults("ap-south-1").security_group.as_deref(), Some("sg-home"));
    }
}
