//! `spotty setup` — log in, create the scoped runtime role, and prepare the
//! account for launching instances.
//!
//! Setup is the one command that runs with the full authority of your SSO
//! permission set. It spends that authority once, to create an IAM role
//! carrying only the permissions spotty actually uses; everything afterwards
//! assumes that role instead. See [`crate::aws::creds`] for the resolution
//! order this establishes.

use anyhow::{bail, Context, Result};
use aws_runtime_async::aws::error::AwsError;
use aws_runtime_async::aws::Client;

use crate::aws::client::AwsClients;
use crate::aws::creds::{self, CredentialSource, DEFAULT_ROLE_NAME};
use crate::aws::instances::ensure_default_security_group;
use crate::aws::sso::{self, BrowserMode};
use crate::config::Config;
use crate::{ec2, iam, sts};

/// Options for [`run`].
#[derive(Debug, Default)]
pub struct SetupOptions {
    pub region: Option<String>,
    /// Profile in `~/.aws/config` to take SSO settings from.
    pub profile: Option<String>,
    /// Name of the scoped role to create.
    pub role_name: Option<String>,
    /// Never try to open a browser (implied when spotty detects no display).
    pub no_browser: bool,
    /// Log in and configure, but make no changes to the AWS account.
    pub dry_run: bool,
    /// Skip creating an SSH key pair and security group.
    pub skip_ec2: bool,
    /// Force a fresh SSO login even if the cached token is still valid.
    pub force_login: bool,
}

/// Run the full setup flow.
pub async fn run(opts: SetupOptions) -> Result<()> {
    let mut config = Config::load().unwrap_or_default();
    let role_name = opts.role_name.clone().unwrap_or_else(|| DEFAULT_ROLE_NAME.to_string());

    // ── 1. Log in ──────────────────────────────────────────────────────
    let profile = opts.profile.clone().or_else(|| config.defaults.sso_profile.clone());
    let sso_config = sso::load_config(profile.as_deref())?;

    println!("IAM Identity Center");
    println!("  start URL : {}", sso_config.start_url);
    println!("  session   : {}", sso_config.session_name);

    let cached = sso::load_cached_token(&sso_config.session_name).filter(|t| t.is_valid());
    let token = match cached {
        Some(t) if !opts.force_login => {
            println!("  token     : valid for {}", format_duration(t.expires_in()));
            t
        }
        _ => {
            let browser = if opts.no_browser { BrowserMode::Never } else { BrowserMode::detect() };
            sso::login(&sso_config, browser).await?
        }
    };

    // ── 2. Elevated credentials, used only in this command ─────────────
    let account_id = sso_config
        .account_id
        .clone()
        .context("profile has no sso_account_id — add one to ~/.aws/config")?;
    let permission_set = sso_config
        .role_name
        .clone()
        .context("profile has no sso_role_name — add one to ~/.aws/config")?;

    let region = creds::resolve_region(opts.region.clone(), &config, Some(&sso_config));

    let base = sso::role_credentials(&sso_config, &token, &account_id, &permission_set).await?;
    let admin = Client::new(aws_runtime_async::aws::sign::Credentials {
        region: region.clone(),
        access_key: base.access_key_id.clone(),
        secret_key: base.secret_access_key.clone(),
        session_token: base.session_token.clone(),
    });

    let identity = sts::get_caller_identity(&admin, sts::GetCallerIdentityRequest::default())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("GetCallerIdentity failed — the SSO token was accepted but STS rejected it")?;

    println!();
    println!("Signed in");
    println!("  account   : {}", identity.account.as_deref().unwrap_or("-"));
    println!("  identity  : {}", identity.arn.as_deref().unwrap_or("-"));
    println!("  region    : {region}");

    // ── 3. The scoped role ─────────────────────────────────────────────
    let role_arn = format!("arn:aws:iam::{account_id}:role/{role_name}");
    let trust = trust_policy(&account_id);
    let permissions = permission_policy(&account_id);

    println!();
    println!("Runtime role");
    println!("  name      : {role_name}");
    println!("  arn       : {role_arn}");
    println!("  grants    : EC2 spot instances, AMIs, Lambda (spotty-* only), SSM parameters");
    println!("  denies    : IAM administration, and touching instances or AMIs spotty did not tag");

    if opts.dry_run {
        println!();
        println!("--dry-run: no changes made. The role would be created as:");
        println!();
        println!("Trust policy:\n{trust}");
        println!();
        println!("Permission policy:\n{permissions}");
        println!();
        println!("To create it by hand:");
        println!("  aws iam create-role --role-name {role_name} \\");
        println!("      --assume-role-policy-document '<trust policy above>'");
        println!("  aws iam put-role-policy --role-name {role_name} \\");
        println!("      --policy-name spotty --policy-document '<permission policy above>'");
        println!("  spotty config set role_arn {role_arn}");
        return Ok(());
    }

    ensure_role(&admin, &role_name, &trust, &permissions).await?;

    config.defaults.role_arn = Some(role_arn.clone());
    config.defaults.region = Some(region.clone());
    if let Some(p) = profile {
        config.defaults.sso_profile = Some(p);
    }
    config.save()?;

    // ── 4. EC2 prerequisites, via the scoped role ──────────────────────
    if !opts.skip_ec2 {
        println!();
        println!("EC2 prerequisites");

        // The role was created moments ago; IAM is eventually consistent, so
        // the first assume can fail even though everything is correct.
        let clients = assume_with_retry(Some(region.clone())).await?;

        let sg = ensure_default_security_group(&clients)
            .await
            .context("failed to prepare the spotty-default security group")?;
        println!("  security group : {sg}");

        let key = match ensure_key_pair(&clients, &role_name).await {
            Ok(Some((name, path))) => {
                println!("  key pair       : {name} ({path})");
                Some((name, path))
            }
            Ok(None) => {
                println!("  key pair       : none created");
                None
            }
            Err(e) => {
                println!("  key pair       : skipped ({e})");
                None
            }
        };

        // Both belong to this region alone — see `Config::region_defaults`.
        let defaults = config.region_defaults_mut(&region);
        defaults.security_group = Some(sg);
        if let Some((name, path)) = key {
            defaults.key_name = Some(name);
            defaults.key_path = Some(path);
        }
        config.save()?;
    }

    println!();
    println!("Setup complete. Commands now run as {role_name}, not {permission_set}.");
    println!("Check anytime with:  spotty setup --status");
    Ok(())
}

/// Create or update the scoped role. Idempotent: re-running setup refreshes
/// both policies so a widened action list takes effect.
async fn ensure_role(admin: &Client, role_name: &str, trust: &str, permissions: &str) -> Result<()> {
    // Only NoSuchEntity means "absent". Treating every failure as absent turns
    // a throttle or a denied GetRole into a CreateRole that fails with
    // EntityAlreadyExists, hiding the real problem.
    let exists = match iam::get_role(admin, iam::GetRoleRequest::default().role_name(role_name)).await {
        Ok(_) => true,
        Err(AwsError::AwsService { ref code, .. }) if code == "NoSuchEntity" => false,
        Err(e) => {
            return Err(anyhow::anyhow!("{e}"))
                .with_context(|| format!("GetRole failed for '{role_name}'"))
        }
    };

    if exists {
        print!("  updating existing role... ");
        iam::update_assume_role_policy(
            admin,
            iam::UpdateAssumeRolePolicyRequest::default()
                .role_name(role_name)
                .policy_document(trust),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("UpdateAssumeRolePolicy failed")?;
    } else {
        print!("  creating role... ");
        iam::create_role(
            admin,
            iam::CreateRoleRequest::default()
                .role_name(role_name)
                .description("Scoped runtime role for the spotty CLI")
                .assume_role_policy_document(trust)
                // Role chaining from an SSO role caps sessions at one hour.
                .max_session_duration(3600),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("CreateRole failed")?;
    }

    iam::put_role_policy(
        admin,
        iam::PutRolePolicyRequest::default()
            .role_name(role_name)
            .policy_name("spotty")
            .policy_document(permissions),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("PutRolePolicy failed")?;

    println!("done");
    Ok(())
}

/// IAM propagation means a freshly created role is not immediately assumable.
async fn assume_with_retry(region: Option<String>) -> Result<AwsClients> {
    let mut delay = std::time::Duration::from_secs(1);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        match AwsClients::new(region.clone()).await {
            Ok(c) => return Ok(c),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("could not assume the spotty role")))
}

/// Create an SSH key pair if the account has none that spotty can use.
async fn ensure_key_pair(clients: &AwsClients, role_name: &str) -> Result<Option<(String, String)>> {
    let key_name = match short_hostname() {
        Some(host) => format!("{role_name}-key-{host}"),
        None => format!("{role_name}-key"),
    };

    let existing = ec2::describe_key_pairs(
        &clients.client,
        ec2::DescribeKeyPairsRequest::default().key_names(&key_name),
    )
    .await;

    let ssh_path = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".ssh")
        .join(format!("{key_name}.pem"));

    if let Ok(resp) = existing {
        if resp.key_pairs.as_ref().is_some_and(|l| !l.item.is_empty()) {
            // AWS only returns key material at creation time, so a key that
            // exists remotely without a local file cannot be recovered.
            if ssh_path.exists() {
                return Ok(Some((key_name, ssh_path.display().to_string())));
            }
            bail!(
                "key pair '{key_name}' exists in AWS but {} is missing; \
                 delete the key pair or point key_path at the private key",
                ssh_path.display()
            );
        }
    }

    crate::keys::create_key_pair(&key_name, Some(clients.region.clone())).await?;
    Ok(Some((key_name, ssh_path.display().to_string())))
}

/// Report what spotty would use right now, without changing anything.
pub async fn status(region: Option<String>) -> Result<()> {
    let config = Config::load().unwrap_or_default();

    println!("Configuration");
    println!("  config file : {}", crate::config::path().display());
    println!("  region      : {}", config.defaults.region.as_deref().unwrap_or("(unset)"));
    println!("  role_arn    : {}", config.defaults.role_arn.as_deref().unwrap_or("(unset)"));

    println!();
    println!("IAM Identity Center");
    match sso::load_config(config.defaults.sso_profile.as_deref()) {
        Ok(c) => {
            println!("  session     : {}", c.session_name);
            println!("  start URL   : {}", c.start_url);
            match sso::load_cached_token(&c.session_name) {
                Some(t) if t.is_valid() => {
                    println!("  token       : valid for {}", format_duration(t.expires_in()))
                }
                Some(_) => println!("  token       : EXPIRED — run `spotty setup`"),
                None => println!("  token       : none — run `spotty setup`"),
            }
        }
        Err(e) => println!("  not configured: {e}"),
    }

    println!();
    println!("Credentials");
    match creds::resolve(region, creds::Privilege::Scoped).await {
        Ok(resolved) => {
            println!("  source      : {}", resolved.source);
            println!("  region      : {}", resolved.credentials.region);

            let client = Client::new(resolved.credentials);
            match sts::get_caller_identity(&client, sts::GetCallerIdentityRequest::default()).await {
                Ok(id) => {
                    println!("  account     : {}", id.account.as_deref().unwrap_or("-"));
                    println!("  identity    : {}", id.arn.as_deref().unwrap_or("-"));
                }
                Err(e) => println!("  identity    : call failed: {e}"),
            }

            if let CredentialSource::PermissionSet { .. } = resolved.source {
                println!();
                println!("  Note: running with full permission-set privileges.");
                println!("  Run `spotty setup` to create and use a scoped role instead.");
            }
        }
        Err(e) => println!("  unavailable : {e}"),
    }

    Ok(())
}

/// Check that the current credentials actually carry the permissions spotty
/// needs, rather than discovering it halfway through a launch.
///
/// Read permissions are checked by making the (harmless) read call. The one
/// mutating permission that matters, `ec2:RunInstances`, is checked with EC2's
/// `DryRun` flag: AWS evaluates authorization and then refuses to do the work,
/// reporting `DryRunOperation` when the caller would have been allowed.
pub async fn preflight(region: Option<String>) -> Result<()> {
    let clients = AwsClients::new(region).await?;

    let identity =
        sts::get_caller_identity(&clients.client, sts::GetCallerIdentityRequest::default())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("GetCallerIdentity failed")?;

    println!(
        "Checking permissions for {} in {}",
        identity.arn.as_deref().unwrap_or("-"),
        clients.region
    );
    println!();

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut check = |action: &str, outcome: Result<()>| {
        checked += 1;
        match outcome {
            Ok(()) => println!("  ok      {action}"),
            Err(e) => {
                println!("  DENIED  {action}: {e}");
                failures.push(action.to_string());
            }
        }
    };

    check("sts:GetCallerIdentity", Ok(()));

    check(
        "ec2:DescribeInstances",
        probe(ec2::describe_instances(
            &clients.client,
            ec2::DescribeInstancesRequest::default().max_results(5),
        ))
        .await,
    );
    check(
        "ec2:DescribeKeyPairs",
        probe(ec2::describe_key_pairs(
            &clients.client,
            ec2::DescribeKeyPairsRequest::default(),
        ))
        .await,
    );
    check(
        "ec2:DescribeSecurityGroups",
        probe(ec2::describe_security_groups(
            &clients.client,
            ec2::DescribeSecurityGroupsRequest::default().max_results(5),
        ))
        .await,
    );
    check(
        "ec2:DescribeVpcs",
        probe(ec2::describe_vpcs(&clients.client, ec2::DescribeVpcsRequest::default())).await,
    );
    check(
        "ec2:DescribeImages",
        probe(ec2::describe_images(
            &clients.client,
            ec2::DescribeImagesRequest::default().max_results(5).owners("self"),
        ))
        .await,
    );
    check(
        "ec2:DescribeSpotPriceHistory",
        probe(ec2::describe_spot_price_history(
            &clients.client,
            ec2::DescribeSpotPriceHistoryRequest::default().max_results(1),
        ))
        .await,
    );
    check(
        "ssm:GetParameter",
        probe(crate::ssm::get_parameter(
            &clients.client,
            crate::ssm::GetParameterRequest::default()
                .name("/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"),
        ))
        .await,
    );
    check(
        "lambda:ListFunctions",
        probe(crate::lambda_api::list_functions(
            &clients.client,
            crate::lambda_api::ListFunctionsRequest::default(),
        ))
        .await,
    );

    // The mutating check: authorized, but never actually launched.
    check("ec2:RunInstances (dry run)", run_instances_dry_run(&clients).await);

    println!();
    if !failures.is_empty() {
        println!("Re-run `spotty setup` to refresh the role's policy, or");
        println!("`spotty setup --dry-run` to see the policy it would apply.");
        println!();
        // A non-zero exit, so `spotty setup --check` can gate a script.
        bail!("{} of {} permission check(s) failed: {}", failures.len(), checked, failures.join(", "));
    }
    println!("All {checked} checks passed.");
    Ok(())
}

/// Run a read call purely for its authorization outcome.
async fn probe<T>(
    call: impl std::future::Future<Output = Result<T, aws_runtime_async::aws::error::AwsError>>,
) -> Result<()> {
    call.await.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Ask EC2 whether this caller could launch, without launching.
async fn run_instances_dry_run(clients: &AwsClients) -> Result<()> {
    let ami = crate::ssm::get_parameter(
        &clients.client,
        crate::ssm::GetParameterRequest::default()
            .name("/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("cannot resolve an AMI to test with: {e}"))?
    .parameter
    .and_then(|p| p.value)
    .context("SSM returned no AMI id")?;

    let result = ec2::run_instances(
        &clients.client,
        ec2::RunInstancesRequest::default()
            .dry_run(true)
            .image_id(ami)
            .instance_type(ec2::InstanceType::from("t3.micro"))
            .min_count(1)
            .max_count(1),
    )
    .await;

    match result {
        // A dry run that reached the work is a pass; it should never succeed.
        Err(aws_runtime_async::aws::error::AwsError::AwsService { ref code, .. })
            if code == "DryRunOperation" =>
        {
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// Print shell exports carrying the scoped, short-lived credentials, for
/// pasting into a remote shell or CI job that should not run its own login.
pub async fn export(region: Option<String>) -> Result<()> {
    let resolved = creds::resolve(region, creds::Privilege::Scoped).await?;
    let c = &resolved.credentials;

    eprintln!("# Scoped credentials from {}.", resolved.source);
    eprintln!("# These are secrets: they grant spotty's permissions until they expire.");
    println!("export AWS_ACCESS_KEY_ID={}", c.access_key);
    println!("export AWS_SECRET_ACCESS_KEY={}", c.secret_key);
    if let Some(ref t) = c.session_token {
        println!("export AWS_SESSION_TOKEN={t}");
    }
    println!("export AWS_REGION={}", c.region);
    Ok(())
}

/// Forget cached credentials. The SSO token itself is left alone, since it is
/// shared with the AWS CLI.
pub fn logout() -> Result<()> {
    creds::clear_cache()?;
    println!("Cleared cached role credentials.");
    println!("The SSO token is shared with the aws CLI; remove it with `aws sso logout`.");
    Ok(())
}

// ── Policies ───────────────────────────────────────────────────────────

/// Trust policy for the scoped role.
///
/// Delegating to the account root is the standard IAM pattern: it does not
/// grant anything by itself: a principal must *also* be allowed `sts:AssumeRole`
/// by its own policy, which for this account means your SSO permission set.
fn trust_policy(account_id: &str) -> String {
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Effect": "Allow",
      "Principal": {{ "AWS": "arn:aws:iam::{account_id}:root" }},
      "Action": "sts:AssumeRole"
    }}
  ]
}}"#
    )
}

/// Permission policy for the scoped role: what spotty needs and nothing more.
///
/// Destructive actions are narrowed wherever EC2 supports it — terminating,
/// imaging and deregistering are restricted to resources spotty itself tagged,
/// so the role cannot touch instances or AMIs belonging to anything else in the
/// account. `CreateImage` needs a second, unconditional statement because IAM
/// also authorizes the image and snapshots it is about to create, which do not
/// carry tags yet.
fn permission_policy(account_id: &str) -> String {
    format!(
        r#"{{
  "Version": "2012-10-17",
  "Statement": [
    {{
      "Sid": "ReadOnlyDiscovery",
      "Effect": "Allow",
      "Action": [
        "ec2:DescribeSpotPriceHistory",
        "ec2:DescribeInstanceTypes",
        "ec2:DescribeInstances",
        "ec2:DescribeImages",
        "ec2:DescribeSnapshots",
        "ec2:DescribeKeyPairs",
        "ec2:DescribeSecurityGroups",
        "ec2:DescribeVpcs",
        "ssm:GetParameter",
        "ssm:GetParameters",
        "sts:GetCallerIdentity"
      ],
      "Resource": "*"
    }},
    {{
      "Sid": "LaunchInstances",
      "Effect": "Allow",
      "Action": [
        "ec2:RunInstances",
        "ec2:CreateKeyPair",
        "ec2:CreateSecurityGroup",
        "ec2:AuthorizeSecurityGroupIngress"
      ],
      "Resource": "*"
    }},
    {{
      "Sid": "TagOnCreate",
      "Effect": "Allow",
      "Action": "ec2:CreateTags",
      "Resource": "*",
      "Condition": {{
        "StringEquals": {{
          "ec2:CreateAction": ["RunInstances", "CreateImage", "CreateSecurityGroup"]
        }}
      }}
    }},
    {{
      "Sid": "ManageOwnInstancesOnly",
      "Effect": "Allow",
      "Action": [
        "ec2:TerminateInstances",
        "ec2:CreateImage"
      ],
      "Resource": "arn:aws:ec2:*:{account_id}:instance/*",
      "Condition": {{
        "StringEquals": {{ "ec2:ResourceTag/ManagedBy": "spotty" }}
      }}
    }},
    {{
      "Sid": "CreateImageArtifacts",
      "Effect": "Allow",
      "Action": "ec2:CreateImage",
      "Resource": [
        "arn:aws:ec2:*::image/*",
        "arn:aws:ec2:*::snapshot/*"
      ]
    }},
    {{
      "Sid": "ManageOwnKeyPairs",
      "Effect": "Allow",
      "Action": "ec2:DeleteKeyPair",
      "Resource": "arn:aws:ec2:*:{account_id}:key-pair/spotty-*"
    }},
    {{
      "Sid": "ManageOwnImages",
      "Effect": "Allow",
      "Action": [
        "ec2:DeregisterImage",
        "ec2:DeleteSnapshot"
      ],
      "Resource": "*",
      "Condition": {{
        "StringEquals": {{ "ec2:ResourceTag/ManagedBy": "spotty" }}
      }}
    }},
    {{
      "Sid": "SpottyLambdaFunctionsOnly",
      "Effect": "Allow",
      "Action": [
        "lambda:CreateFunction",
        "lambda:DeleteFunction",
        "lambda:GetFunction",
        "lambda:GetFunctionConfiguration",
        "lambda:InvokeFunction",
        "lambda:UpdateFunctionCode",
        "lambda:UpdateFunctionConfiguration",
        "lambda:TagResource",
        "lambda:ListTags"
      ],
      "Resource": "arn:aws:lambda:*:{account_id}:function:spotty-*"
    }},
    {{
      "Sid": "ListLambdaFunctions",
      "Effect": "Allow",
      "Action": "lambda:ListFunctions",
      "Resource": "*"
    }},
    {{
      "Sid": "LambdaExecutionRoleOnly",
      "Effect": "Allow",
      "Action": [
        "iam:CreateRole",
        "iam:GetRole",
        "iam:AttachRolePolicy",
        "iam:PassRole"
      ],
      "Resource": "arn:aws:iam::{account_id}:role/spotty-lambda-execution"
    }}
  ]
}}"#
    )
}

// ── Helpers ────────────────────────────────────────────────────────────

/// This machine's short hostname, reduced to characters that read well in a
/// key-pair name. `None` when the hostname cannot be determined.
///
/// Key pairs are named per machine because AWS only hands out the private key
/// once, at creation: a single shared name means whichever machine ran `setup`
/// second has a key pair it cannot use.
fn short_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let sanitized: String = raw
        .trim()
        // Strip the domain: `tink.local` and `tink` are the same machine.
        .split('.')
        .next()?
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(sanitized).filter(|s| !s.is_empty())
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_are_valid_json() {
        let trust: serde_json::Value = serde_json::from_str(&trust_policy("123456789012")).unwrap();
        assert_eq!(trust["Statement"][0]["Action"], "sts:AssumeRole");

        let perms: serde_json::Value =
            serde_json::from_str(&permission_policy("123456789012")).unwrap();
        let statements = perms["Statement"].as_array().unwrap();
        assert!(!statements.is_empty());
        // Termination must stay tag-scoped: an unconditional statement here
        // would let the role kill instances it does not own.
        let terminate = statements
            .iter()
            .find(|s| s["Sid"] == "ManageOwnInstancesOnly")
            .expect("terminate statement");
        assert_eq!(
            terminate["Condition"]["StringEquals"]["ec2:ResourceTag/ManagedBy"],
            "spotty"
        );
        assert_eq!(terminate["Resource"], "arn:aws:ec2:*:123456789012:instance/*");
    }

    #[test]
    fn deleting_images_stays_tag_scoped() {
        let perms: serde_json::Value =
            serde_json::from_str(&permission_policy("123456789012")).unwrap();
        let images = perms["Statement"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["Sid"] == "ManageOwnImages")
            .expect("image statement");
        // Unconditional DeregisterImage/DeleteSnapshot on "*" would let the
        // role delete any AMI in the account, which is what the printed
        // "denies" summary promises it cannot do.
        assert_eq!(
            images["Condition"]["StringEquals"]["ec2:ResourceTag/ManagedBy"],
            "spotty"
        );
    }

    #[test]
    fn create_image_can_reach_its_new_resources() {
        let perms: serde_json::Value =
            serde_json::from_str(&permission_policy("123456789012")).unwrap();
        // IAM authorizes the image and snapshots CreateImage is about to
        // create, not just the source instance. They carry no tags yet, so a
        // tag condition on them denies every `spotty image create`.
        let artifacts = perms["Statement"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["Sid"] == "CreateImageArtifacts")
            .expect("create-image artifact statement");
        assert!(artifacts["Condition"].is_null());
        let resources = artifacts["Resource"].as_array().unwrap();
        assert!(resources.iter().any(|r| r == "arn:aws:ec2:*::image/*"));
        assert!(resources.iter().any(|r| r == "arn:aws:ec2:*::snapshot/*"));
    }

    #[test]
    fn permission_policy_scopes_lambda_to_account() {
        let policy = permission_policy("123456789012");
        assert!(policy.contains("arn:aws:lambda:*:123456789012:function:spotty-*"));
        assert!(!policy.contains("\"lambda:*\""));
    }

    #[test]
    fn hostname_is_usable_in_a_key_name() {
        // Whatever this machine is called, the result has to be a legal,
        // readable key-pair name — no dots, spaces or case surprises.
        if let Some(h) = short_hostname() {
            assert!(!h.is_empty());
            assert!(h.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{h}");
        }
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(5400), "1h 30m");
        assert_eq!(format_duration(300), "5m");
    }
}
