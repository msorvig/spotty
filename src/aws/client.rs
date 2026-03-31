use anyhow::Result;
use aws_runtime_async::aws::Client;

use crate::config::Config;

pub struct AwsClients {
    pub client: Client,
    pub region: String,
}

impl AwsClients {
    pub fn new(region: Option<String>) -> Result<Self> {
        let spotty_config = Config::load().unwrap_or_default();

        // Use CLI region, then spotty config region, then let aws-gen resolve
        let explicit_region = region.or(spotty_config.defaults.region.clone());

        let mut client = Client::from_env();

        if let Some(ref r) = explicit_region {
            client.creds.region = r.clone();
        }

        let region_str = client.creds.region.clone();

        Ok(Self {
            client,
            region: region_str,
        })
    }
}
