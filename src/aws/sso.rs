//! IAM Identity Center (AWS SSO) login via the OAuth 2.0 device
//! authorization grant.
//!
//! The device grant is designed for input-constrained devices, which makes it
//! the same flow whether spotty runs on a desktop or over SSH: the user code
//! it prints is a public one-time code, so nothing secret has to be carried
//! between machines and no port forwarding is needed.
//!
//! Access tokens are cached in the same place and format the AWS CLI uses
//! (`~/.aws/sso/cache/<sha1(session)>.json`), so a login here is visible to
//! `aws` and vice versa.

use anyhow::{bail, Context, Result};
use aws_runtime_async::aws::error::AwsError;
use aws_runtime_async::aws::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sso_oidc;

/// How spotty identifies itself when registering an OIDC client.
const CLIENT_NAME: &str = "spotty";
/// The only scope spotty needs: enumerate accounts/roles and mint role creds.
const SCOPE: &str = "sso:account:access";
/// Polling floor, used when the service does not supply an interval.
const DEFAULT_POLL_INTERVAL: u64 = 5;

/// An `[sso-session NAME]` block from `~/.aws/config`, plus the account and
/// role the active profile pins it to.
#[derive(Debug, Clone)]
pub struct SsoConfig {
    pub session_name: String,
    pub start_url: String,
    pub sso_region: String,
    pub account_id: Option<String>,
    pub role_name: Option<String>,
    /// Region for EC2/STS calls, which is not necessarily `sso_region`.
    pub region: Option<String>,
}

/// A cached SSO access token, in the AWS CLI's on-disk format.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedToken {
    #[serde(rename = "startUrl")]
    pub start_url: String,
    pub region: String,
    #[serde(rename = "accessToken")]
    pub access_token: String,
    /// RFC 3339 UTC, e.g. `2026-08-29T13:04:05Z`.
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(rename = "refreshToken", skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl CachedToken {
    /// True when the token is still valid, with a minute of slack so a token
    /// does not expire midway through the command that just loaded it.
    pub fn is_valid(&self) -> bool {
        match parse_rfc3339(&self.expires_at) {
            Some(exp) => exp > now_secs() + 60,
            None => false,
        }
    }

    /// Seconds until expiry, saturating at zero.
    pub fn expires_in(&self) -> u64 {
        parse_rfc3339(&self.expires_at)
            .map(|exp| exp.saturating_sub(now_secs()))
            .unwrap_or(0)
    }
}

// ── ~/.aws/config parsing ──────────────────────────────────────────────

fn aws_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("AWS_CONFIG_FILE") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".aws").join("config"))
}

/// Parse an INI file into (section name, key/value pairs).
fn parse_ini(content: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Vec::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            if let Some((_, kvs)) = sections.last_mut() {
                kvs.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    sections
}

/// Resolve the SSO configuration for `profile` (defaulting to `$AWS_PROFILE`,
/// then `default`) out of `~/.aws/config`.
pub fn load_config(profile: Option<&str>) -> Result<SsoConfig> {
    let path = aws_config_path()?;
    if !path.exists() {
        bail!(
            "no AWS config at {}. Create an IAM Identity Center profile with \
             `aws configure sso`, then run `spotty setup`.",
            path.display()
        );
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let sections = parse_ini(&content);

    let profile = profile
        .map(String::from)
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .unwrap_or_else(|| "default".to_string());

    // The `default` profile is spelled `[default]`; all others `[profile X]`.
    let wanted: [String; 2] = [format!("profile {profile}"), profile.clone()];
    let prof = sections
        .iter()
        .find(|(name, _)| wanted.iter().any(|w| w == name))
        .map(|(_, kvs)| kvs)
        .with_context(|| format!("profile '{profile}' not found in {}", path.display()))?;

    let get = |kvs: &[(String, String)], key: &str| -> Option<String> {
        kvs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };

    let session_name = get(prof, "sso_session").with_context(|| {
        format!(
            "profile '{profile}' has no sso_session. Legacy `sso_start_url` profiles are not \
             supported; add an [sso-session] block, or use env-var credentials."
        )
    })?;

    let session = sections
        .iter()
        .find(|(name, _)| name == &format!("sso-session {session_name}"))
        .map(|(_, kvs)| kvs)
        .with_context(|| format!("[sso-session {session_name}] not found in {}", path.display()))?;

    Ok(SsoConfig {
        start_url: get(session, "sso_start_url")
            .with_context(|| format!("[sso-session {session_name}] has no sso_start_url"))?,
        sso_region: get(session, "sso_region")
            .with_context(|| format!("[sso-session {session_name}] has no sso_region"))?,
        account_id: get(prof, "sso_account_id"),
        role_name: get(prof, "sso_role_name"),
        region: get(prof, "region"),
        session_name,
    })
}

// ── Token cache ────────────────────────────────────────────────────────

fn token_cache_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".aws").join("sso").join("cache"))
}

/// The AWS CLI keys its SSO token cache by SHA-1 of the session name.
fn token_cache_path(session_name: &str) -> Result<PathBuf> {
    Ok(token_cache_dir()?.join(format!("{}.json", hex_sha1(session_name.as_bytes()))))
}

pub fn load_cached_token(session_name: &str) -> Option<CachedToken> {
    let path = token_cache_path(session_name).ok()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cached_token(session_name: &str, token: &CachedToken) -> Result<()> {
    crate::utils::write_secret(&token_cache_path(session_name)?, &serde_json::to_string_pretty(token)?)
}

/// The cached token for `config`, renewed if it has run out.
///
/// IAM Identity Center hands out a short-lived access token — an hour is
/// typical — alongside a refresh token good for the rest of the SSO session.
/// Without spending the refresh token, spotty would send you back to the
/// browser every hour, while `aws` in the next terminal carries on.
///
/// `None` means a real login is needed: no token cached, or the refresh was
/// refused because the session itself has ended.
pub async fn load_or_refresh_token(config: &SsoConfig) -> Option<CachedToken> {
    let cached = load_cached_token(&config.session_name)?;
    if cached.is_valid() {
        return Some(cached);
    }

    match refresh_token(config, &cached).await {
        Ok(fresh) => Some(fresh),
        Err(e) => {
            tracing::debug!("SSO token refresh failed, falling back to login: {e}");
            None
        }
    }
}

/// Exchange the refresh token for a new access token.
async fn refresh_token(config: &SsoConfig, stale: &CachedToken) -> Result<CachedToken> {
    let client_id = stale.client_id.as_deref().context("cached token has no clientId")?;
    let client_secret = stale
        .client_secret
        .as_deref()
        .context("cached token has no clientSecret")?;
    let refresh = stale
        .refresh_token
        .as_deref()
        .context("cached token has no refreshToken")?;

    let client = Client::anonymous(&config.sso_region);
    let resp = sso_oidc::create_token(
        &client,
        sso_oidc::CreateTokenRequest::default()
            .client_id(client_id)
            .client_secret(client_secret)
            .grant_type("refresh_token")
            .refresh_token(refresh),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("CreateToken(refresh_token) failed")?;

    let cached = CachedToken {
        start_url: config.start_url.clone(),
        region: config.sso_region.clone(),
        access_token: resp.access_token.context("refresh returned no accessToken")?,
        expires_at: format_rfc3339(now_secs() + resp.expires_in.unwrap_or(3600).max(0) as u64),
        client_id: Some(client_id.to_string()),
        client_secret: Some(client_secret.to_string()),
        // Refresh tokens are rotated: keep the new one, or the next refresh
        // is rejected and the hourly browser trip comes back.
        refresh_token: resp.refresh_token.or_else(|| stale.refresh_token.clone()),
    };
    save_cached_token(&config.session_name, &cached)?;
    Ok(cached)
}

// ── Device authorization flow ──────────────────────────────────────────

/// How the verification URL should reach the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    /// Try to open a browser, printing the URL as well.
    Auto,
    /// Only print the URL — for SSH sessions and CI.
    Never,
}

impl BrowserMode {
    /// `Never` when there is no plausible browser to open: an SSH session, or
    /// a machine with no display server.
    pub fn detect() -> Self {
        let headless = std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_TTY").is_some()
            || (cfg!(target_os = "linux")
                && std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none());
        if headless {
            BrowserMode::Never
        } else {
            BrowserMode::Auto
        }
    }
}

/// Run the device authorization grant and return a freshly cached token.
///
/// Blocks until the user approves in a browser, the code expires, or the user
/// interrupts.
pub async fn login(config: &SsoConfig, browser: BrowserMode) -> Result<CachedToken> {
    let client = Client::anonymous(&config.sso_region);

    // Step 1: register this installation as an OIDC client. The registration
    // is short-lived and unauthenticated, so it is not worth caching.
    let reg = sso_oidc::register_client(
        &client,
        sso_oidc::RegisterClientRequest::default()
            .client_name(CLIENT_NAME)
            .client_type("public")
            .scopes(SCOPE),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("RegisterClient failed")?;

    let client_id = reg.client_id.context("RegisterClient returned no clientId")?;
    let client_secret = reg
        .client_secret
        .context("RegisterClient returned no clientSecret")?;

    // Step 2: start device authorization and show the user their code.
    let auth = sso_oidc::start_device_authorization(
        &client,
        sso_oidc::StartDeviceAuthorizationRequest::default()
            .client_id(&client_id)
            .client_secret(&client_secret)
            .start_url(&config.start_url),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("StartDeviceAuthorization failed")?;

    let device_code = auth
        .device_code
        .context("StartDeviceAuthorization returned no deviceCode")?;
    let user_code = auth.user_code.unwrap_or_default();
    let verification_uri = auth.verification_uri.unwrap_or_default();
    let verification_uri_complete = auth
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| format!("{verification_uri}?user_code={user_code}"));

    println!();
    println!("Confirm this login in a browser:");
    println!();
    println!("  {verification_uri_complete}");
    println!();
    println!("  (or open {verification_uri} and enter code {user_code})");
    println!();

    if browser == BrowserMode::Auto {
        if open_browser(&verification_uri_complete) {
            println!("Opened your browser.");
        }
    } else {
        println!("No browser on this machine — open the link on any device.");
    }
    println!("Waiting for approval (Ctrl-C to cancel)...");

    // Step 3: poll for the token. The service dictates the interval and can
    // ask us to back off further with slow_down.
    let mut interval = auth.interval.unwrap_or(DEFAULT_POLL_INTERVAL as i32).max(1) as u64;
    let deadline = now_secs() + auth.expires_in.unwrap_or(600).max(1) as u64;

    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;

        if now_secs() >= deadline {
            bail!("login timed out — the code expired before it was approved");
        }

        let result = sso_oidc::create_token(
            &client,
            sso_oidc::CreateTokenRequest::default()
                .client_id(&client_id)
                .client_secret(&client_secret)
                .grant_type("urn:ietf:params:oauth:grant-type:device_code")
                .device_code(&device_code),
        )
        .await;

        match result {
            Ok(token) => {
                let access_token = token
                    .access_token
                    .context("CreateToken returned no accessToken")?;
                let expires_at =
                    format_rfc3339(now_secs() + token.expires_in.unwrap_or(3600).max(0) as u64);

                let cached = CachedToken {
                    start_url: config.start_url.clone(),
                    region: config.sso_region.clone(),
                    access_token,
                    expires_at,
                    client_id: Some(client_id),
                    client_secret: Some(client_secret),
                    refresh_token: token.refresh_token,
                };
                save_cached_token(&config.session_name, &cached)?;
                println!("Approved.");
                return Ok(cached);
            }
            Err(AwsError::AwsService { ref code, .. }) if is_pending(code) => continue,
            Err(AwsError::AwsService { ref code, .. }) if is_slow_down(code) => {
                interval += 5;
                continue;
            }
            Err(AwsError::AwsService { code, message }) => {
                bail!("login failed ({code}): {message}");
            }
            // A dropped connection or an unreadable body is not the service
            // saying no. Giving up here would throw away an approval the user
            // has already given, so keep polling until the code expires.
            Err(e) => {
                tracing::debug!("transient error while polling for approval: {e}");
                continue;
            }
        }
    }
}

/// Botocore models these as exceptions; the wire format is the OAuth spelling.
fn is_pending(code: &str) -> bool {
    code == "authorization_pending" || code == "AuthorizationPendingException"
}

fn is_slow_down(code: &str) -> bool {
    code == "slow_down" || code == "SlowDownException"
}

/// Exchange a cached SSO token for temporary credentials on a permission-set
/// role.
pub async fn role_credentials(
    config: &SsoConfig,
    token: &CachedToken,
    account_id: &str,
    role_name: &str,
) -> Result<crate::aws::creds::TempCredentials> {
    let client = Client::anonymous(&config.sso_region);

    let resp = crate::sso_portal::get_role_credentials(
        &client,
        crate::sso_portal::GetRoleCredentialsRequest {
            role_name: role_name.to_string(),
            account_id: account_id.to_string(),
            access_token: token.access_token.clone(),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("GetRoleCredentials failed for {role_name} in {account_id}"))?;

    let rc = resp
        .role_credentials
        .context("GetRoleCredentials returned no credentials")?;

    Ok(crate::aws::creds::TempCredentials {
        access_key_id: rc.access_key_id.context("no accessKeyId")?,
        secret_access_key: rc.secret_access_key.context("no secretAccessKey")?,
        session_token: rc.session_token,
        // The portal reports expiry in milliseconds since the epoch.
        expires_at: rc.expiration.map(|ms| (ms / 1000) as u64).unwrap_or(0),
    })
}

// ── Helpers ────────────────────────────────────────────────────────────

fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_rfc3339(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn parse_rfc3339(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

/// SHA-1, needed only to match the AWS CLI's token cache file naming.
fn hex_sha1(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64) * 8;

    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    h.iter().map(|w| format!("{w:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        // The AWS CLI names its token cache files by SHA-1 of the session
        // name, so a wrong digest here silently orphans the cache.
        assert_eq!(hex_sha1(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex_sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex_sha1(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn parses_sso_session_and_profile() {
        let ini = parse_ini(
            "[default]\nregion = eu-north-1\nsso_session = s\nsso_account_id = 123\n\
             [sso-session s]\nsso_start_url = https://example.awsapps.com/start/\nsso_region = eu-north-1\n",
        );
        assert_eq!(ini.len(), 2);
        assert_eq!(ini[0].0, "default");
        assert_eq!(ini[1].0, "sso-session s");
        assert!(ini[1].1.iter().any(|(k, _)| k == "sso_start_url"));
    }

    #[test]
    fn ini_ignores_comments_and_blanks() {
        let ini = parse_ini("# comment\n\n[a]\n; another\nk = v\n");
        assert_eq!(ini.len(), 1);
        assert_eq!(ini[0].1, vec![("k".to_string(), "v".to_string())]);
    }

    #[test]
    fn token_validity_uses_expiry() {
        let mk = |expires_at: String| CachedToken {
            start_url: String::new(),
            region: String::new(),
            access_token: String::new(),
            expires_at,
            client_id: None,
            client_secret: None,
            refresh_token: None,
        };
        assert!(mk(format_rfc3339(now_secs() + 3600)).is_valid());
        assert!(!mk(format_rfc3339(now_secs() + 10)).is_valid(), "inside the 60s slack");
        assert!(!mk(format_rfc3339(now_secs() - 1)).is_valid());
        assert!(!mk("not a date".into()).is_valid());
    }
}
