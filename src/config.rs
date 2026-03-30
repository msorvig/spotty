use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub ami: std::collections::HashMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Defaults {
    pub region: Option<String>,
    pub key_name: Option<String>,
    pub key_path: Option<String>,
    pub ssh_user: Option<String>,
    pub security_group: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config from {:?}", path))?;
            toml::from_str(&content).with_context(|| "Failed to parse config file")
        } else {
            Ok(Config::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spotty")
        .join("config.toml")
}

pub fn show_config() -> Result<()> {
    let _config = Config::load()?;
    let path = config_path();

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
    let path = config_path();

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
        },
        ami: std::collections::HashMap::new(),
    };

    config.save()?;
    println!("Created config file at {:?}", path);

    Ok(())
}
