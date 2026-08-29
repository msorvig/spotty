//! AWS Lambda operations for spotty.
//!
//! Manages Lambda functions as a compute backend — deploy binaries,
//! invoke them, and manage their lifecycle.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;
use std::process::Command;
use tabled::Tabled;

use crate::aws::client::AwsClients;
use crate::lambda_api;

const SPOTTY_TAG: &str = "spotty";

/// Configuration for deploying a Lambda function.
pub struct LambdaDeployConfig {
    pub binary_path: String,
    pub name: Option<String>,
    pub memory: i32,
    pub timeout: i32,
    pub arch: Option<String>,
    pub region: Option<String>,
}

/// Info about a Lambda function.
#[derive(Debug, Serialize, Tabled)]
pub struct LambdaFunctionInfo {
    #[tabled(rename = "NAME")]
    pub name: String,
    #[tabled(rename = "RUNTIME")]
    pub runtime: String,
    #[tabled(rename = "MEMORY")]
    pub memory: String,
    #[tabled(rename = "TIMEOUT")]
    pub timeout: String,
    #[tabled(rename = "ARCH")]
    pub arch: String,
    #[tabled(rename = "MODIFIED")]
    pub last_modified: String,
    #[tabled(skip)]
    pub arn: String,
    #[tabled(skip)]
    pub state: String,
    #[tabled(skip)]
    pub code_size: i64,
    #[tabled(skip)]
    pub handler: String,
}

/// Result from invoking a Lambda function.
#[derive(Debug)]
pub struct InvokeResult {
    pub payload: Option<Vec<u8>>,
    pub function_error: Option<String>,
    pub log_tail: Option<String>,
    pub status_code: Option<i32>,
}

/// Ensure the spotty Lambda execution role exists, return its ARN.
async fn ensure_lambda_role(clients: &AwsClients) -> Result<String> {
    let role_name = "spotty-lambda-execution";

    // Check if role already exists
    let get_req = crate::iam::GetRoleRequest::default()
        .role_name(role_name.to_string());
    match crate::iam::get_role(&clients.client, get_req).await {
        Ok(resp) => {
            let arn = resp.role.arn;
            if !arn.is_empty() {
                return Ok(arn);
            }
        }
        Err(_) => {
            // Role doesn't exist, create it
        }
    }

    eprintln!("Creating IAM role: {}...", role_name);

    let trust_policy = r#"{
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Principal": {"Service": "lambda.amazonaws.com"},
            "Action": "sts:AssumeRole"
        }]
    }"#;

    let create_req = crate::iam::CreateRoleRequest::default()
        .role_name(role_name.to_string())
        .assume_role_policy_document(trust_policy.to_string());
    let create_resp = crate::iam::create_role(&clients.client, create_req)
        .await
        .context("Failed to create Lambda execution role")?;

    let arn = create_resp.role.arn;
    if arn.is_empty() {
        bail!("CreateRole returned no ARN");
    }

    // Attach basic execution policy (CloudWatch Logs)
    let attach_req = crate::iam::AttachRolePolicyRequest::default()
        .role_name(role_name.to_string())
        .policy_arn("arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole".to_string());
    crate::iam::attach_role_policy(&clients.client, attach_req)
        .await
        .context("Failed to attach execution policy")?;

    // IAM role propagation delay — Lambda can't use it immediately
    eprintln!("Waiting for IAM role to propagate...");
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    Ok(arn)
}

/// Package a binary into a zip file with the binary renamed to `bootstrap`.
fn package_binary(binary_path: &str) -> Result<Vec<u8>> {
    let path = Path::new(binary_path);
    if !path.exists() {
        bail!("Binary not found: {}", binary_path);
    }

    let tmp_dir = std::env::temp_dir();
    let zip_path = tmp_dir.join("spotty-lambda-package.zip");

    let bootstrap_path = tmp_dir.join("bootstrap");
    if bootstrap_path.exists() {
        std::fs::remove_file(&bootstrap_path)?;
    }
    std::fs::copy(path, &bootstrap_path)
        .context("Failed to copy binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bootstrap_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let status = Command::new("zip")
        .arg("-j")
        .arg(&zip_path)
        .arg(&bootstrap_path)
        .status()
        .context("Failed to run zip command. Is zip installed?")?;

    if !status.success() {
        bail!("zip command failed");
    }

    let zip_bytes = std::fs::read(&zip_path)?;

    let _ = std::fs::remove_file(&bootstrap_path);
    let _ = std::fs::remove_file(&zip_path);

    let size_mb = zip_bytes.len() as f64 / (1024.0 * 1024.0);
    if size_mb > 50.0 {
        bail!(
            "Zip package is {:.1} MB, exceeding Lambda's 50 MB limit. \
             Consider using a container image instead.",
            size_mb
        );
    }

    Ok(zip_bytes)
}

/// Generate a function name from the binary path.
fn default_function_name(binary_path: &str) -> String {
    let stem = Path::new(binary_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "function".to_string());
    format!("spotty-{}", stem)
}

/// Deploy a binary as a Lambda function (create or update).
/// Returns the function name.
pub async fn deploy_function(config: LambdaDeployConfig) -> Result<String> {
    let clients = AwsClients::new(config.region.clone()).await?;
    let function_name = config.name.clone()
        .unwrap_or_else(|| default_function_name(&config.binary_path));

    let arch = config.arch.as_deref().unwrap_or("x86_64");
    let lambda_arch = match arch {
        "arm64" => lambda_api::Architecture::Arm64,
        _ => lambda_api::Architecture::X8664,
    };

    let zip_bytes = package_binary(&config.binary_path)?;

    // Check if function already exists
    let get_req = lambda_api::GetFunctionRequest::default()
        .function_name(function_name.clone());
    let exists = lambda_api::get_function(&clients.client, get_req).await.is_ok();

    if exists {
        let update_req = lambda_api::UpdateFunctionCodeRequest::default()
            .function_name(function_name.clone())
            .zip_file(zip_bytes)
            .architectures(lambda_arch);
        lambda_api::update_function_code(&clients.client, update_req)
            .await
            .context("Failed to update function code")?;

        let config_req = lambda_api::UpdateFunctionConfigurationRequest::default()
            .function_name(function_name.clone())
            .timeout(config.timeout)
            .memory_size(config.memory);
        lambda_api::update_function_configuration(&clients.client, config_req)
            .await
            .context("Failed to update function configuration")?;
    } else {
        let role_arn = ensure_lambda_role(&clients).await?;

        let mut tags = std::collections::HashMap::new();
        tags.insert("ManagedBy".to_string(), SPOTTY_TAG.to_string());

        let create_req = lambda_api::CreateFunctionRequest::default()
            .function_name(function_name.clone())
            .runtime(lambda_api::Runtime::ProvidedAl2023)
            .handler("bootstrap".to_string())
            .role(role_arn)
            .code(lambda_api::FunctionCode::default().zip_file(zip_bytes))
            .timeout(config.timeout)
            .memory_size(config.memory)
            .architectures(lambda_arch)
            .package_type(lambda_api::PackageType::Zip)
            .tags(tags);

        lambda_api::create_function(&clients.client, create_req)
            .await
            .context("Failed to create function")?;
    }

    Ok(function_name)
}

/// Invoke a Lambda function. Returns the invocation result.
pub async fn invoke_function(
    name: String,
    payload: Option<String>,
    async_mode: bool,
    region: Option<String>,
) -> Result<InvokeResult> {
    let clients = AwsClients::new(region).await?;

    let invocation_type = if async_mode {
        lambda_api::InvocationType::Event
    } else {
        lambda_api::InvocationType::RequestResponse
    };

    let mut req = lambda_api::InvocationRequest::default()
        .function_name(name)
        .invocation_type(invocation_type);

    if let Some(ref p) = payload {
        req = req.payload(p.as_bytes().to_vec());
    }

    let resp = lambda_api::invoke(&clients.client, req)
        .await
        .context("Failed to invoke function")?;

    let log_tail = resp.log_result.map(|log| {
        let decoded = aws_runtime_async::base64::decode(&log);
        String::from_utf8_lossy(&decoded).to_string()
    });

    Ok(InvokeResult {
        payload: resp.payload,
        function_error: resp.function_error,
        log_tail,
        status_code: resp.status_code,
    })
}

/// List spotty-managed Lambda functions.
pub async fn list_functions(region: Option<String>) -> Result<Vec<LambdaFunctionInfo>> {
    let clients = AwsClients::new(region).await?;

    let req = lambda_api::ListFunctionsRequest::default();
    let resp = lambda_api::list_functions(&clients.client, req)
        .await
        .context("Failed to list functions")?;

    let functions = resp.functions.map(|f| f.item).unwrap_or_default();

    let mut result = Vec::new();
    for func in &functions {
        let name = func.function_name.as_deref().unwrap_or("");
        let arn = func.function_arn.as_deref().unwrap_or("");

        let mut is_spotty = name.starts_with("spotty-");
        if !is_spotty && !arn.is_empty() {
            let tags_req = lambda_api::ListTagsRequest::default()
                .resource(arn.to_string());
            if let Ok(tags_resp) = lambda_api::list_tags(&clients.client, tags_req).await {
                if let Some(tags) = tags_resp.tags {
                    is_spotty = tags.get("ManagedBy").map(|v| v.as_str()) == Some(SPOTTY_TAG);
                }
            }
        }

        if is_spotty {
            result.push(function_config_to_info(func));
        }
    }

    Ok(result)
}

/// Delete Lambda function(s). Returns list of deleted names.
pub async fn delete_functions(
    names: Vec<String>,
    all: bool,
    region: Option<String>,
) -> Result<Vec<String>> {
    let clients = AwsClients::new(region.clone()).await?;

    let to_delete = if all {
        let funcs = list_functions(region).await?;
        funcs.into_iter().map(|f| f.name).collect()
    } else {
        if names.is_empty() {
            bail!("Provide function name(s) or use --all");
        }
        names
    };

    if to_delete.is_empty() {
        return Ok(Vec::new());
    }

    for name in &to_delete {
        let req = lambda_api::DeleteFunctionRequest::default()
            .function_name(name.clone());
        lambda_api::delete_function(&clients.client, req)
            .await
            .with_context(|| format!("Failed to delete function: {}", name))?;
    }

    Ok(to_delete)
}

/// Get status of a Lambda function.
pub async fn function_status(name: String, region: Option<String>) -> Result<LambdaFunctionInfo> {
    let clients = AwsClients::new(region).await?;

    let req = lambda_api::GetFunctionConfigurationRequest::default()
        .function_name(name);
    let config = lambda_api::get_function_configuration(&clients.client, req)
        .await
        .context("Failed to get function configuration")?;

    Ok(function_config_to_info(&config))
}

fn function_config_to_info(config: &lambda_api::FunctionConfiguration) -> LambdaFunctionInfo {
    LambdaFunctionInfo {
        name: config.function_name.clone().unwrap_or_default(),
        runtime: config.runtime.as_ref().map(|r| r.to_string()).unwrap_or_default(),
        memory: config.memory_size.map(|m| format!("{} MB", m)).unwrap_or_default(),
        timeout: config.timeout.map(|t| format!("{}s", t)).unwrap_or_default(),
        arch: config.architectures.as_ref()
            .and_then(|a| a.item.first())
            .map(|a| a.to_string())
            .unwrap_or_default(),
        last_modified: config.last_modified.clone().unwrap_or_default(),
        arn: config.function_arn.clone().unwrap_or_default(),
        state: config.state.as_ref().map(|s| s.to_string()).unwrap_or_default(),
        code_size: config.code_size.unwrap_or(0),
        handler: config.handler.clone().unwrap_or_default(),
    }
}
