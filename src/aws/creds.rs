//! Credential resolution.
//!
//! Day-to-day commands should not run with whatever privileges your SSO
//! permission set happens to carry — usually far more than spotty needs. So
//! the chain deliberately prefers a *scoped* credential:
//!
//! 1. `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` — an explicit override always wins.
//! 2. Cached spotty role credentials, if still valid.
//! 3. A cached SSO token → permission-set credentials → `sts:AssumeRole` into
//!    the `spotty` role created by `spotty setup`. The permission-set
//!    credentials are used for that one call and never written to disk.
//! 4. `aws configure export-credentials`, for setups that do not use SSO.
//!
//! [`Privilege::Elevated`] stops after step 3's permission-set credentials —
//! `spotty setup` needs them to create the role in the first place.

use anyhow::{bail, Context, Result};
use aws_runtime_async::aws::sign::Credentials;
use aws_runtime_async::aws::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::aws::sso;
use crate::config::Config;
use crate::sts;

/// Default name of the scoped role `spotty setup` creates.
pub const DEFAULT_ROLE_NAME: &str = "spotty";
/// Assumed-role session lifetime. Role chaining from an SSO role caps at 1h.
const SESSION_DURATION_SECS: i32 = 3600;

/// Temporary credentials plus their expiry, as seconds since the epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub expires_at: u64,
}

impl TempCredentials {
    /// True while the credentials have more than five minutes left, so a
    /// long-running command does not expire midway.
    pub fn is_valid(&self) -> bool {
        self.expires_at > sso::now_secs() + 300
    }

    fn into_aws(self, region: String) -> Credentials {
        Credentials {
            region,
            access_key: self.access_key_id,
            secret_key: self.secret_access_key,
            session_token: self.session_token,
        }
    }
}

/// How much authority the caller needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// The scoped `spotty` role — everything except `setup`.
    Scoped,
    /// The SSO permission set itself, for creating the scoped role.
    Elevated,
}

/// Where a resolved credential came from, for `spotty setup --status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    /// The scoped role, cached locally.
    ScopedRole(String),
    /// The SSO permission set, used directly.
    PermissionSet { account_id: String, role_name: String },
    AwsCli,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialSource::Environment => write!(f, "environment variables"),
            CredentialSource::ScopedRole(arn) => write!(f, "scoped role {arn}"),
            CredentialSource::PermissionSet { account_id, role_name } => {
                write!(f, "SSO permission set {role_name} in {account_id}")
            }
            CredentialSource::AwsCli => write!(f, "aws CLI"),
        }
    }
}

pub struct Resolved {
    pub credentials: Credentials,
    pub source: CredentialSource,
}

// ── Cache ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialCache {
    /// Keyed by `"{role_arn}|{region}"`.
    #[serde(default)]
    roles: HashMap<String, TempCredentials>,
}

fn cache_path() -> Result<PathBuf> {
    let dir = dirs::cache_dir().context("cannot determine cache directory")?;
    Ok(dir.join("spotty").join("credentials.json"))
}

fn load_cache() -> CredentialCache {
    cache_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CredentialCache) -> Result<()> {
    crate::utils::write_secret(&cache_path()?, &serde_json::to_string_pretty(cache)?)
}

/// Drop every cached credential — `spotty setup --logout`.
pub fn clear_cache() -> Result<()> {
    let path = cache_path()?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

// ── Resolution ─────────────────────────────────────────────────────────

/// Resolve credentials for `region`, preferring the least-privileged source
/// that can satisfy `privilege`.
pub async fn resolve(region: Option<String>, privilege: Privilege) -> Result<Resolved> {
    let config = Config::load().unwrap_or_default();

    // 1. An explicit environment override always wins, at whatever privilege
    //    it happens to carry. Read the variables directly: `try_from_env`
    //    prefers `aws configure export-credentials`, which would quietly hand
    //    back a different account than the one the environment names.
    if std::env::var("AWS_ACCESS_KEY_ID").is_ok() {
        let mut creds = Credentials::from_env_vars().map_err(|e| anyhow::anyhow!("{e}"))?;
        creds.region = resolve_region(region, &config, None);
        return Ok(Resolved { credentials: creds, source: CredentialSource::Environment });
    }

    let sso_config = sso::load_config(config.defaults.sso_profile.as_deref()).ok();
    let region = resolve_region(region, &config, sso_config.as_ref());

    let role_arn = config.defaults.role_arn.clone();

    // 2. A cached scoped-role credential, if one is still good.
    if privilege == Privilege::Scoped {
        if let Some(ref arn) = role_arn {
            let cache = load_cache();
            if let Some(cached) = cache.roles.get(&cache_key(arn, &region)) {
                if cached.is_valid() {
                    return Ok(Resolved {
                        credentials: cached.clone().into_aws(region),
                        source: CredentialSource::ScopedRole(arn.clone()),
                    });
                }
            }
        }
    }

    // 3. SSO: cached token → permission-set credentials → scoped role.
    if let Some(ref sso_config) = sso_config {
        if let Some(token) = sso::load_or_refresh_token(sso_config).await {
            let account_id = sso_config
                .account_id
                .clone()
                .context("profile has no sso_account_id; set one or run `spotty setup`")?;
            let permission_set = sso_config
                .role_name
                .clone()
                .context("profile has no sso_role_name; set one or run `spotty setup`")?;

            let base =
                sso::role_credentials(sso_config, &token, &account_id, &permission_set).await?;

            if privilege == Privilege::Elevated {
                return Ok(Resolved {
                    credentials: base.into_aws(region),
                    source: CredentialSource::PermissionSet { account_id, role_name: permission_set },
                });
            }

            match role_arn {
                Some(arn) => {
                    let scoped = assume_role(&base, &region, &arn).await?;
                    let mut cache = load_cache();
                    cache.roles.insert(cache_key(&arn, &region), scoped.clone());
                    save_cache(&cache)?;
                    return Ok(Resolved {
                        credentials: scoped.into_aws(region),
                        source: CredentialSource::ScopedRole(arn),
                    });
                }
                // No scoped role configured yet. Usable, but this is the
                // over-privileged path `setup` exists to get you off.
                None => {
                    return Ok(Resolved {
                        credentials: base.into_aws(region),
                        source: CredentialSource::PermissionSet { account_id, role_name: permission_set },
                    });
                }
            }
        }
    }

    // 4. Non-SSO setups that keep credentials in ~/.aws/credentials.
    if let Ok(mut creds) = Credentials::try_from_env() {
        creds.region = region;
        return Ok(Resolved { credentials: creds, source: CredentialSource::AwsCli });
    }

    bail!(not_logged_in_message(sso_config.as_ref()))
}

fn cache_key(role_arn: &str, region: &str) -> String {
    format!("{role_arn}|{region}")
}

/// The region to operate in, most specific source first.
///
/// The order follows the AWS CLI: an explicit `--region`, then the
/// environment, then stored configuration. `AWS_REGION` has to outrank
/// `config.toml`, because `spotty setup` always writes a region there —
/// leaving it below would make exporting `AWS_REGION` do nothing at all once
/// setup had run. With nothing else to go on, the region the SSO session
/// itself lives in beats a hardcoded us-east-1.
pub fn resolve_region(
    explicit: Option<String>,
    config: &Config,
    sso_config: Option<&sso::SsoConfig>,
) -> String {
    explicit
        .or_else(Credentials::region_from_env)
        .or_else(|| config.defaults.region.clone())
        .or_else(|| sso_config.and_then(|c| c.region.clone()))
        .or_else(|| sso_config.map(|c| c.sso_region.clone()))
        .unwrap_or_else(Credentials::resolve_region)
}

/// Exchange base credentials for the scoped role's credentials.
async fn assume_role(
    base: &TempCredentials,
    region: &str,
    role_arn: &str,
) -> Result<TempCredentials> {
    let client = Client::new(base.clone().into_aws(region.to_string()));

    let resp = sts::assume_role(
        &client,
        sts::AssumeRoleRequest::default()
            .role_arn(role_arn)
            .role_session_name("spotty")
            .duration_seconds(SESSION_DURATION_SECS),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| {
        format!(
            "failed to assume {role_arn}. If the role is missing or its trust policy \
             changed, re-run `spotty setup`."
        )
    })?;

    let c = resp.credentials.context("AssumeRole returned no credentials")?;
    Ok(TempCredentials {
        access_key_id: c.access_key_id,
        secret_access_key: c.secret_access_key,
        session_token: Some(c.session_token),
        expires_at: chrono::DateTime::parse_from_rfc3339(&c.expiration)
            .map(|dt| dt.timestamp().max(0) as u64)
            // Trust the requested duration if STS sends a format we cannot read.
            .unwrap_or_else(|_| sso::now_secs() + SESSION_DURATION_SECS as u64),
    })
}

fn not_logged_in_message(sso_config: Option<&sso::SsoConfig>) -> String {
    match sso_config {
        Some(c) => format!(
            "not logged in to {} (session '{}').\n\nRun:\n  spotty setup",
            c.start_url, c.session_name
        ),
        None => "no AWS credentials found.\n\nRun:\n  spotty setup\n\n\
                 Or set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(expires_at: u64) -> TempCredentials {
        TempCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
            session_token: None,
            expires_at,
        }
    }

    #[test]
    fn validity_leaves_headroom() {
        assert!(creds(sso::now_secs() + 3600).is_valid());
        // Inside the five-minute margin: treated as expired so a long command
        // does not lose its credentials halfway through.
        assert!(!creds(sso::now_secs() + 60).is_valid());
        assert!(!creds(0).is_valid());
    }

    #[test]
    fn cache_key_separates_regions() {
        assert_ne!(cache_key("arn:aws:iam::1:role/spotty", "eu-north-1"),
                   cache_key("arn:aws:iam::1:role/spotty", "us-east-1"));
    }
}
