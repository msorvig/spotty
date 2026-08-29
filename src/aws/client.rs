use anyhow::Result;
use aws_runtime_async::aws::Client;

use crate::aws::creds::{self, CredentialSource, Privilege};

pub struct AwsClients {
    pub client: Client,
    pub region: String,
    /// Which credential source backed this client, for `setup --status` and
    /// for telling the user when they are running over-privileged.
    pub source: CredentialSource,
}

impl AwsClients {
    /// A client using the scoped `spotty` role where one is configured.
    pub async fn new(region: Option<String>) -> Result<Self> {
        Self::with_privilege(region, Privilege::Scoped).await
    }

    /// A client using the SSO permission set directly. Only `spotty setup`
    /// should need this — it is what creates the scoped role.
    pub async fn elevated(region: Option<String>) -> Result<Self> {
        Self::with_privilege(region, Privilege::Elevated).await
    }

    async fn with_privilege(region: Option<String>, privilege: Privilege) -> Result<Self> {
        let resolved = creds::resolve(region, privilege).await?;
        let region = resolved.credentials.region.clone();
        Ok(Self {
            client: Client::new(resolved.credentials),
            region,
            source: resolved.source,
        })
    }
}
