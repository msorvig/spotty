use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ssm::Client as SsmClient;

use crate::config::Config;

#[allow(dead_code)]
pub struct AwsClients {
    pub ec2: Ec2Client,
    pub ssm: SsmClient,
    pub region: String,
}

impl AwsClients {
    pub async fn new(region: Option<String>) -> Result<Self> {
        let spotty_config = Config::load().unwrap_or_default();

        // Use CLI region, then spotty config region, then let AWS SDK resolve
        let explicit_region = region.or(spotty_config.defaults.region.clone());

        let aws_config = if let Some(ref r) = explicit_region {
            aws_config::defaults(BehaviorVersion::latest())
                .region(aws_config::Region::new(r.clone()))
                .load()
                .await
        } else {
            aws_config::load_defaults(BehaviorVersion::latest()).await
        };

        let region_str = aws_config
            .region()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "us-east-1".to_string());

        let ec2 = Ec2Client::new(&aws_config);
        let ssm = SsmClient::new(&aws_config);

        Ok(Self {
            ec2,
            ssm,
            region: region_str,
        })
    }
}
