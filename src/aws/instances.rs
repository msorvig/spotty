use anyhow::{bail, Context, Result};
use aws_runtime_async::aws::error::AwsError;
use chrono::Utc;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::time::Duration;
use tabled::Tabled;

use super::client::AwsClients;
use crate::config::Config;
use crate::ec2;
use crate::utils::base64_encode;

// Common EC2 filter builders to reduce duplication
fn filter_managed_by_spotty() -> ec2::Filter {
    ec2::Filter::default()
        .name("tag:ManagedBy")
        .values("spotty")
}

fn filter_running_or_pending() -> ec2::Filter {
    ec2::Filter::default()
        .name("instance-state-name")
        .values("pending")
        .values("running")
}

fn filter_active_states() -> ec2::Filter {
    ec2::Filter::default()
        .name("instance-state-name")
        .values("pending")
        .values("running")
        .values("stopping")
        .values("stopped")
}

fn filter_auto_cleanup_true() -> ec2::Filter {
    ec2::Filter::default()
        .name("tag:AutoCleanup")
        .values("true")
}

#[derive(Debug, Serialize, Tabled)]
pub struct InstanceInfo {
    #[tabled(rename = "Instance ID")]
    pub instance_id: String,
    #[tabled(rename = "Name")]
    pub name: String,
    #[tabled(rename = "Type")]
    pub instance_type: String,
    #[tabled(rename = "Arch")]
    pub arch: String,
    #[tabled(rename = "vCPU")]
    pub vcpu: i32,
    #[tabled(rename = "State")]
    pub state: String,
    #[tabled(rename = "Public IP")]
    pub public_ip: String,
    #[tabled(rename = "Rate/hr")]
    pub rate: String,
    #[tabled(rename = "Cost")]
    pub cost: String,
    #[tabled(rename = "Running")]
    pub running_time: String,
    #[tabled(skip)]
    #[serde(skip)]
    pub running_secs: u64,
}

/// Configuration for launching instances - reduces parameter count
#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub instance_type: Option<String>,
    pub vcpu_min: Option<i32>,
    pub vcpu_max: Option<i32>,
    pub memory_min: Option<f64>,
    pub memory_max: Option<f64>,
    pub arch: Option<String>,
    pub max_price: Option<f64>,
    pub ami: Option<String>,
    pub region: Option<String>,
    pub key_name: Option<String>,
    pub security_group: Option<String>,
    pub name: Option<String>,
    pub user_data: Option<String>,
    pub no_auto_cleanup: bool,
    pub max_tries: usize,
    pub volume_size: Option<i32>,
}

/// Result from launching instances
struct LaunchResult {
    instance_ids: Vec<String>,
}

/// Core implementation for launching instances - shared between start_instances and start_instance_and_get_id
async fn launch_instances_impl(
    config: &LaunchConfig,
    number: usize,
) -> Result<LaunchResult> {
    let clients = AwsClients::new(config.region.clone())?;
    let spotty_config = Config::load()?;

    // Get candidate instance types
    let candidates = get_instance_candidates(
        &clients,
        config.instance_type.clone(),
        config.vcpu_min,
        config.vcpu_max,
        config.memory_min,
        config.memory_max,
        config.arch.as_deref(),
        config.max_price,
        config.max_tries,
    )
    .await?;

    if candidates.is_empty() {
        bail!("No matching instance types found");
    }

    eprintln!("Found {} candidate instance types, trying from cheapest:", candidates.len());
    for (i, (it, az, price)) in candidates.iter().take(5).enumerate() {
        eprintln!("  {}. {} in {} @ ${:.4}/hr", i + 1, it, az, price);
    }
    if candidates.len() > 5 {
        eprintln!("  ... and {} more", candidates.len() - 5);
    }

    // Resolve key name (do this once before trying instances)
    let key = match config.key_name.clone().or(spotty_config.defaults.key_name.clone()) {
        Some(k) => k,
        None => {
            eprintln!("Discovering SSH key...");
            discover_key_pair(&clients).await?
        }
    };

    // Resolve security group
    let sg = match config.security_group.clone().or(spotty_config.defaults.security_group.clone()) {
        Some(sg) => sg,
        None => {
            eprintln!("Creating default security group...");
            ensure_default_security_group(&clients).await?
        }
    };

    // Read user data if provided
    let user_data_content = if let Some(ref path) = config.user_data {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read user-data file: {}", path))?;
        Some(base64_encode(&content))
    } else {
        None
    };

    // Build tag specification
    let instance_name = config.name.clone().unwrap_or_else(|| "spotty-instance".to_string());
    let auto_cleanup_value = if config.no_auto_cleanup { "false" } else { "true" };

    // Try each candidate until one works
    let mut last_error: Option<anyhow::Error> = None;

    for (instance_type_str, _az, price) in &candidates {
        eprintln!("\nTrying {} @ ${:.4}/hr...", instance_type_str, price);

        // Resolve AMI for this instance type
        let ami_id = match &config.ami {
            Some(id) => id.clone(),
            None => match discover_ami(&clients, instance_type_str).await {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("  Failed to discover AMI: {}", e);
                    last_error = Some(e);
                    continue;
                }
            }
        };

        // Build spot options with max price
        let mut spot_opts = ec2::SpotMarketOptions::default();
        if let Some(mp) = config.max_price {
            spot_opts = spot_opts.max_price(format!("{:.4}", mp));
        }

        let market_opts = ec2::InstanceMarketOptionsRequest::default()
            .market_type(ec2::MarketType::Spot)
            .spot_options(spot_opts);

        let tags = ec2::TagSpecification::default()
            .resource_type(ec2::ResourceType::Instance)
            .tags(ec2::Tag::default().key("Name").value(instance_name.clone()))
            .tags(ec2::Tag::default().key("ManagedBy").value("spotty"))
            .tags(ec2::Tag::default().key("AutoCleanup").value(auto_cleanup_value));

        // Parse instance type
        let it = ec2::InstanceType::from(instance_type_str.as_str());

        // Launch instance(s)
        let mut req = ec2::RunInstancesRequest::default()
            .image_id(ami_id)
            .instance_type(it)
            .key_name(key.clone())
            .security_group_ids(sg.clone())
            .instance_market_options(market_opts)
            .tag_specifications(tags)
            .min_count(number as i32)
            .max_count(number as i32);

        if let Some(ref ud) = user_data_content {
            req = req.user_data(ud.clone());
        }

        if let Some(size) = config.volume_size {
            req = req.block_device_mappings(
                ec2::BlockDeviceMapping::default()
                    .device_name("/dev/xvda")
                    .ebs(
                        ec2::EbsBlockDevice::default()
                            .volume_size(size)
                            .volume_type(ec2::VolumeType::Gp3)
                            .delete_on_termination(true),
                    ),
            );
        }

        match ec2::run_instances(&clients.client, req).await {
            Ok(resp) => {
                let instances: Vec<_> = resp.instances.iter().flat_map(|l| &l.item).collect();
                if instances.is_empty() {
                    eprintln!("  No instances returned");
                    continue;
                }

                let mut instance_ids: Vec<String> = Vec::new();
                for instance in instances {
                    if let Some(ref id) = instance.instance_id {
                        eprintln!("  Instance {} launched!", id);
                        instance_ids.push(id.clone());

                        // Track instance in local state
                        let auto_cleanup = !config.no_auto_cleanup;
                        if let Err(e) = crate::state::track_instance(id, &clients.region, auto_cleanup) {
                            eprintln!("Warning: Failed to track instance: {}", e);
                        }
                    }
                }

                return Ok(LaunchResult { instance_ids });
            }
            Err(e) => {
                let error_str = format!("{}", e);
                if error_str.contains("InsufficientInstanceCapacity") {
                    eprintln!("  No capacity available");
                } else if error_str.contains("SpotMaxPriceTooLow") {
                    eprintln!("  Spot price too low");
                } else {
                    match &e {
                        AwsError::AwsService { code, message } => {
                            eprintln!("  Failed: {} ({})", message, code);
                        }
                        _ => {
                            eprintln!("  Failed: {}", e);
                        }
                    }
                }
                last_error = Some(anyhow::anyhow!("{}", e));
                continue;
            }
        }
    }

    // If we get here, all attempts failed
    bail!(
        "Failed to launch any instance after {} attempts. Last error: {:?}",
        candidates.len().min(config.max_tries),
        last_error
    )
}

/// Start one or more instances with the given configuration.
/// Returns info for all launched instances.
pub async fn start_instances(
    config: LaunchConfig,
    number: usize,
    wait: bool,
) -> Result<Vec<InstanceInfo>> {
    let region = config.region.clone();
    let result = launch_instances_impl(&config, number).await?;
    let clients = AwsClients::new(region)?;

    if wait {
        eprintln!("Waiting for instances to be running...");
        for id in &result.instance_ids {
            wait_for_running(&clients, id).await?;
        }
    }

    // Get final status for all instances
    let mut infos: Vec<InstanceInfo> = Vec::new();
    for id in &result.instance_ids {
        infos.push(get_instance_info(&clients, id).await?);
    }

    Ok(infos)
}

/// Start an instance and return its ID (always waits for running state)
pub async fn start_instance_and_get_id(config: LaunchConfig) -> Result<String> {
    let region = config.region.clone();
    let result = launch_instances_impl(&config, 1).await?;
    let clients = AwsClients::new(region)?;

    let instance_id = result.instance_ids.into_iter().next()
        .ok_or_else(|| anyhow::anyhow!("No instance ID returned"))?;

    eprintln!("Waiting for instance to be running...");
    wait_for_running(&clients, &instance_id).await?;

    Ok(instance_id)
}

/// Get sorted list of (instance_type, az, price) candidates
async fn get_instance_candidates(
    clients: &AwsClients,
    instance_type: Option<String>,
    vcpu_min: Option<i32>,
    vcpu_max: Option<i32>,
    memory_min: Option<f64>,
    memory_max: Option<f64>,
    arch: Option<&str>,
    max_price: Option<f64>,
    limit: usize,
) -> Result<Vec<(String, String, f64)>> {
    use super::filters::{get_instance_types, InstanceTypeFilter};

    // If specific instance type given, just return that
    if let Some(it) = instance_type {
        // Fetch current price for this instance type
        let prices = fetch_spot_prices_for_types(clients, &[it.clone()]).await?;
        if let Some((az, price)) = prices.get(&it) {
            return Ok(vec![(it, az.clone(), *price)]);
        }
        // No price found, still return it (will use on-demand pricing)
        return Ok(vec![(it, String::new(), 0.0)]);
    }

    // Otherwise, fetch all prices and filter
    eprintln!("Fetching spot prices...");
    let (instance_types_result, prices_result) = tokio::join!(
        get_instance_types(clients),
        fetch_all_spot_prices(clients)
    );

    let instance_types = instance_types_result?;
    let prices = prices_result?;

    let filter = InstanceTypeFilter {
        vcpu_min,
        vcpu_max,
        memory_min_gb: memory_min,
        memory_max_gb: memory_max,
        arch: arch.map(String::from),
    };

    let mut results: Vec<(String, String, f64)> = prices
        .into_iter()
        .filter_map(|(it, az, price)| {
            let spec = instance_types.get(&it)?;

            if !filter.matches(spec) {
                return None;
            }

            if let Some(max) = max_price {
                if price > max {
                    return None;
                }
            }

            Some((it, az, price))
        })
        .collect();

    // Sort by price (cheapest first)
    results.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());

    // Deduplicate by instance type (keep cheapest AZ for each type)
    let mut seen: HashSet<String> = HashSet::new();
    results.retain(|(it, _, _)| seen.insert(it.clone()));

    results.truncate(limit);
    Ok(results)
}

async fn fetch_spot_prices_for_types(
    clients: &AwsClients,
    types: &[String],
) -> Result<HashMap<String, (String, f64)>> {
    let mut result = HashMap::new();

    let mut req = ec2::DescribeSpotPriceHistoryRequest::default()
        .start_time(Utc::now().to_rfc3339())
        .filters(
            ec2::Filter::default()
                .name("product-description")
                .values("Linux/UNIX"),
        );

    for it in types {
        req = req.instance_types(ec2::InstanceType::from(it.as_str()));
    }

    let resp = ec2::describe_spot_price_history(&clients.client, req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    for history in resp.spot_price_history.iter().flat_map(|l| &l.item) {
        if let (Some(it), Some(az), Some(price_str)) = (
            &history.instance_type,
            &history.availability_zone,
            &history.spot_price,
        ) {
            if let Ok(price) = price_str.parse::<f64>() {
                let key = it.to_string();
                result
                    .entry(key)
                    .and_modify(|(existing_az, existing_price)| {
                        if price < *existing_price {
                            *existing_az = az.to_string();
                            *existing_price = price;
                        }
                    })
                    .or_insert((az.to_string(), price));
            }
        }
    }

    Ok(result)
}

/// Fetch all spot prices with pagination and deduplication.
/// Used by both instances.rs and pricing.rs.
pub async fn fetch_all_spot_prices(clients: &AwsClients) -> Result<Vec<(String, String, f64)>> {
    let mut prices = Vec::new();
    let mut next_token: Option<String> = None;

    loop {
        let mut req = ec2::DescribeSpotPriceHistoryRequest::default()
            .start_time(Utc::now().to_rfc3339())
            .filters(
                ec2::Filter::default()
                    .name("product-description")
                    .values("Linux/UNIX"),
            );

        if let Some(token) = next_token {
            req = req.next_token(token);
        }

        let resp = ec2::describe_spot_price_history(&clients.client, req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        for history in resp.spot_price_history.iter().flat_map(|l| &l.item) {
            if let (Some(it), Some(az), Some(price_str)) = (
                &history.instance_type,
                &history.availability_zone,
                &history.spot_price,
            ) {
                if let Ok(price) = price_str.parse::<f64>() {
                    prices.push((it.to_string(), az.to_string(), price));
                }
            }
        }

        next_token = resp.next_token.clone();
        if next_token.is_none() {
            break;
        }

        if prices.len() > 5000 {
            break;
        }
    }

    // Deduplicate - keep lowest price per instance type + AZ
    let mut best_prices: HashMap<(String, String), f64> = HashMap::new();
    for (it, az, price) in prices {
        let key = (it.clone(), az.clone());
        best_prices
            .entry(key)
            .and_modify(|p| {
                if price < *p {
                    *p = price;
                }
            })
            .or_insert(price);
    }

    Ok(best_prices
        .into_iter()
        .map(|((it, az), price)| (it, az, price))
        .collect())
}

/// Terminate instances. Returns list of terminated instance IDs.
pub async fn stop_instances(
    mut instance_ids: Vec<String>,
    all: bool,
    region: Option<String>,
) -> Result<Vec<String>> {
    // If no IDs given and not --all, show interactive menu (always, even for single instance)
    if instance_ids.is_empty() && !all {
        let resolved = resolve_instance_selector_no_auto(None, region.clone()).await?;
        if let Some(id) = resolved {
            instance_ids.push(id);
        }
    }

    let clients = AwsClients::new(region)?;

    // If --all, find all spotty-managed running instances
    if all {
        let resp = ec2::describe_instances(
            &clients.client,
            ec2::DescribeInstancesRequest::default()
                .filters(filter_managed_by_spotty())
                .filters(filter_running_or_pending()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        for reservation in resp.reservations.iter().flat_map(|l| &l.item) {
            for instance in reservation.instances.iter().flat_map(|l| &l.item) {
                if let Some(ref id) = instance.instance_id {
                    instance_ids.push(id.clone());
                }
            }
        }
    }

    if instance_ids.is_empty() {
        return Ok(Vec::new());
    }

    let req = ec2::TerminateInstancesRequest {
        instance_ids: ec2::InstanceIdStringList::from(instance_ids.clone()),
        ..Default::default()
    };
    ec2::terminate_instances(&clients.client, req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Untrack terminated instances
    if let Err(e) = crate::state::untrack_instances(&instance_ids) {
        eprintln!("Warning: Failed to untrack instances: {}", e);
    }

    Ok(instance_ids)
}

/// Get status of instances. Returns list of instance info.
pub async fn instance_status(
    instance_ids: Vec<String>,
    all: bool,
    region: Option<String>,
) -> Result<Vec<InstanceInfo>> {
    // Collect regions to query using HashSet for efficient deduplication
    let mut regions_to_query: HashSet<String> = HashSet::new();

    // Add regions from state file
    if let Ok(state) = crate::state::State::load() {
        regions_to_query.extend(state.get_all_regions());
    }

    // Add user's region (let AwsClients resolve from CLI/config/AWS SDK)
    let user_clients = AwsClients::new(region)?;
    regions_to_query.insert(user_clients.region.clone());

    let mut all_instances: Vec<InstanceInfo> = Vec::new();

    for region in &regions_to_query {
        let clients = AwsClients::new(Some(region.clone()))?;

        let mut req = ec2::DescribeInstancesRequest::default();

        if !instance_ids.is_empty() {
            req.instance_ids = Some(ec2::InstanceIdStringList::from(instance_ids.clone()));
        } else {
            req = req.filters(filter_managed_by_spotty());
        }

        if !all {
            req = req.filters(filter_active_states());
        }

        let resp = ec2::describe_instances(&clients.client, req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        // Collect instance types and AZs to fetch spot prices
        let mut spot_queries: Vec<(String, String)> = Vec::new();
        for reservation in resp.reservations.iter().flat_map(|l| &l.item) {
            for instance in reservation.instances.iter().flat_map(|l| &l.item) {
                if let (Some(it), Some(az)) = (
                    instance.instance_type.as_ref().map(|t| t.to_string()),
                    instance.placement.as_ref().and_then(|p| p.availability_zone.as_ref()),
                ) {
                    spot_queries.push((it, az.clone()));
                }
            }
        }

        // Fetch current spot prices and instance type specs
        let (spot_prices, instance_types) = tokio::join!(
            fetch_current_spot_prices(&clients, &spot_queries),
            super::filters::get_instance_types(&clients)
        );
        let spot_prices = spot_prices?;
        let instance_types = instance_types?;

        for reservation in resp.reservations.iter().flat_map(|l| &l.item) {
            for instance in reservation.instances.iter().flat_map(|l| &l.item) {
                all_instances.push(parse_instance(instance, &spot_prices, &instance_types));
            }
        }
    }

    Ok(all_instances)
}

/// Compute status footer values for display.
/// Returns: [total_label, "", "", "", vcpu, "", "", rate, cost, core_time]
pub fn status_footer(instances: &[InstanceInfo]) -> Vec<String> {
    let mut total_rate = 0.0;
    let mut total_cost = 0.0;
    let mut total_vcpu: i32 = 0;
    let mut total_core_secs: u64 = 0;

    for inst in instances {
        if let Some(rate) = inst.rate.strip_prefix('$') {
            if let Ok(r) = rate.parse::<f64>() {
                total_rate += r;
            }
        }
        if let Some(cost) = inst.cost.strip_prefix('$') {
            if let Ok(c) = cost.parse::<f64>() {
                total_cost += c;
            }
        }
        total_vcpu += inst.vcpu;
        total_core_secs += inst.running_secs * inst.vcpu as u64;
    }

    vec![
        format!("Total: {}", instances.len()),
        String::new(),
        String::new(),
        String::new(),
        total_vcpu.to_string(),
        String::new(),
        String::new(),
        format!("${:.4}", total_rate),
        format!("${:.4}", total_cost),
        format_core_time(total_core_secs),
    ]
}

async fn fetch_current_spot_prices(
    clients: &AwsClients,
    queries: &[(String, String)],
) -> Result<HashMap<(String, String), f64>> {
    let mut prices: HashMap<(String, String), f64> = HashMap::new();

    if queries.is_empty() {
        return Ok(prices);
    }

    let instance_types: Vec<_> = queries.iter().map(|(it, _)| it.clone()).collect();

    let mut req = ec2::DescribeSpotPriceHistoryRequest::default()
        .start_time(chrono::Utc::now().to_rfc3339())
        .filters(
            ec2::Filter::default()
                .name("product-description")
                .values("Linux/UNIX"),
        );

    for it in &instance_types {
        req = req.instance_types(ec2::InstanceType::from(it.as_str()));
    }

    let resp = ec2::describe_spot_price_history(&clients.client, req)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    for history in resp.spot_price_history.iter().flat_map(|l| &l.item) {
        if let (Some(it), Some(az), Some(price_str)) = (
            &history.instance_type,
            &history.availability_zone,
            &history.spot_price,
        ) {
            if let Ok(price) = price_str.parse::<f64>() {
                let key = (it.to_string(), az.to_string());
                prices.entry(key).or_insert(price);
            }
        }
    }

    Ok(prices)
}

async fn discover_key_pair(clients: &AwsClients) -> Result<String> {
    let resp = ec2::describe_key_pairs(
        &clients.client,
        ec2::DescribeKeyPairsRequest::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let keys: Vec<_> = resp
        .key_pairs
        .iter()
        .flat_map(|l| &l.item)
        .filter_map(|k| k.key_name.as_deref())
        .collect();

    match keys.len() {
        0 => bail!("No SSH key pairs found. Create one with: aws ec2 create-key-pair --key-name <name>"),
        1 => {
            eprintln!("Using key: {}", keys[0]);
            Ok(keys[0].to_string())
        }
        _ => {
            eprintln!("Using key: {} (of {} available)", keys[0], keys.len());
            Ok(keys[0].to_string())
        }
    }
}

async fn discover_ami(clients: &AwsClients, instance_type: &str) -> Result<String> {
    // Get architecture from cached instance type data
    let instance_types = super::filters::get_instance_types(clients).await?;
    let arch = instance_types
        .get(instance_type)
        .and_then(|spec| spec.architectures.first())
        .map(|a| a.as_str())
        .unwrap_or("x86_64");

    let param_name = format!(
        "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-{}",
        arch
    );

    let resp = crate::ssm::get_parameter(
        &clients.client,
        crate::ssm::GetParameterRequest::default().name(param_name.clone()),
    )
    .await
    .with_context(|| format!("Failed to discover AMI for {}", arch))?;

    let ami = resp
        .parameter
        .as_ref()
        .and_then(|p| p.value.as_deref())
        .ok_or_else(|| anyhow::anyhow!("No AMI found for {}", arch))?;

    eprintln!("Using AMI: {} ({})", ami, arch);
    Ok(ami.to_string())
}

async fn ensure_default_security_group(clients: &AwsClients) -> Result<String> {
    // Check if spotty-default exists
    let resp = ec2::describe_security_groups(
        &clients.client,
        ec2::DescribeSecurityGroupsRequest::default()
            .filters(
                ec2::Filter::default()
                    .name("group-name")
                    .values("spotty-default"),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    if let Some(sg) = resp.security_groups.as_ref().and_then(|l| l.item.first()) {
        if let Some(ref id) = sg.group_id {
            return Ok(id.clone());
        }
    }

    // Get default VPC
    let vpcs = ec2::describe_vpcs(
        &clients.client,
        ec2::DescribeVpcsRequest::default()
            .filters(
                ec2::Filter::default()
                    .name("is-default")
                    .values("true"),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let vpc_id = vpcs
        .vpcs
        .as_ref()
        .and_then(|l| l.item.first())
        .and_then(|v| v.vpc_id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("No default VPC found"))?;

    // Create security group
    let create_resp = ec2::create_security_group(
        &clients.client,
        ec2::CreateSecurityGroupRequest::default()
            .group_name("spotty-default")
            .description("Default security group for spotty instances")
            .vpc_id(vpc_id),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let sg_id = create_resp
        .group_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Failed to create security group"))?;

    // Add SSH ingress rule (0.0.0.0/0 - user should restrict this)
    ec2::authorize_security_group_ingress(
        &clients.client,
        ec2::AuthorizeSecurityGroupIngressRequest::default()
            .group_id(sg_id.clone())
            .ip_protocol("tcp")
            .from_port(22)
            .to_port(22)
            .cidr_ip("0.0.0.0/0"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    eprintln!("Created security group: {} (allows SSH from anywhere)", sg_id);
    Ok(sg_id.clone())
}

async fn wait_for_running(clients: &AwsClients, instance_id: &str) -> Result<()> {
    loop {
        let resp = ec2::describe_instances(
            &clients.client,
            ec2::DescribeInstancesRequest::default()
                .instance_ids(instance_id),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        let state = resp
            .reservations
            .as_ref()
            .and_then(|l| l.item.first())
            .and_then(|r| r.instances.as_ref())
            .and_then(|l| l.item.first())
            .and_then(|i| i.state.as_ref())
            .and_then(|s| s.name.as_ref())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        match state.as_str() {
            "running" => {
                eprintln!("Instance is running");
                return Ok(());
            }
            "pending" => {
                eprint!(".");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ => {
                bail!("Instance entered unexpected state: {}", state);
            }
        }
    }
}

async fn get_instance_info(clients: &AwsClients, instance_id: &str) -> Result<InstanceInfo> {
    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .instance_ids(instance_id),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let instance = resp
        .reservations
        .as_ref()
        .and_then(|l| l.item.first())
        .and_then(|r| r.instances.as_ref())
        .and_then(|l| l.item.first())
        .ok_or_else(|| anyhow::anyhow!("Instance not found"))?;

    Ok(parse_instance(instance, &HashMap::new(), &HashMap::new()))
}

pub async fn get_instance_public_ip(
    clients: &AwsClients,
    instance_id: &str,
) -> Result<String> {
    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .instance_ids(instance_id),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let ip = resp
        .reservations
        .as_ref()
        .and_then(|l| l.item.first())
        .and_then(|r| r.instances.as_ref())
        .and_then(|l| l.item.first())
        .and_then(|i| i.public_ip_address.as_deref())
        .ok_or_else(|| anyhow::anyhow!("Instance has no public IP"))?;

    Ok(ip.to_string())
}

pub async fn get_instance_key_name(
    clients: &AwsClients,
    instance_id: &str,
) -> Result<String> {
    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .instance_ids(instance_id),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let key_name = resp
        .reservations
        .as_ref()
        .and_then(|l| l.item.first())
        .and_then(|r| r.instances.as_ref())
        .and_then(|l| l.item.first())
        .and_then(|i| i.key_name.as_deref())
        .ok_or_else(|| anyhow::anyhow!("Instance has no key name"))?;

    Ok(key_name.to_string())
}

fn parse_instance(
    instance: &ec2::Instance,
    spot_prices: &HashMap<(String, String), f64>,
    instance_types: &HashMap<String, super::filters::InstanceTypeSpec>,
) -> InstanceInfo {
    let name = instance
        .tags
        .as_ref()
        .iter()
        .flat_map(|l| &l.item)
        .find(|t| t.key.as_deref() == Some("Name"))
        .and_then(|t| t.value.as_deref())
        .unwrap_or("-")
        .to_string();

    let instance_type = instance
        .instance_type
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string());

    let (vcpu, arch) = instance_types
        .get(&instance_type)
        .map(|spec| (spec.vcpu, spec.architectures.first().cloned().unwrap_or_default()))
        .unwrap_or((0, "-".to_string()));

    let az = instance
        .placement
        .as_ref()
        .and_then(|p| p.availability_zone.as_deref())
        .unwrap_or("-");

    // Get spot price for this instance type + AZ
    let price_per_hour = spot_prices
        .get(&(instance_type.clone(), az.to_string()))
        .copied();

    // Calculate running time and cost
    let (running_time, running_secs, cost, rate) = if let Some(ref launch) = instance.launch_time {
        // Parse launch time string to get seconds
        let launch_secs = chrono::DateTime::parse_from_rfc3339(launch)
            .or_else(|_| chrono::DateTime::parse_from_str(launch, "%Y-%m-%dT%H:%M:%S%.fZ"))
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        let now_secs = Utc::now().timestamp();
        let running_secs = (now_secs - launch_secs).max(0) as u64;

        // AWS bills minimum 1 minute, then per-second after that
        let billable_secs = running_secs.max(60);
        let billable_hours = billable_secs as f64 / 3600.0;

        let running_str = format_duration(running_secs);

        if let Some(price) = price_per_hour {
            let cost_val = billable_hours * price;
            (
                running_str,
                running_secs,
                format!("${:.4}", cost_val),
                format!("${:.4}", price),
            )
        } else {
            (running_str, running_secs, "-".to_string(), "-".to_string())
        }
    } else {
        ("-".to_string(), 0, "-".to_string(), "-".to_string())
    };

    InstanceInfo {
        instance_id: instance.instance_id.as_deref().unwrap_or("-").to_string(),
        name,
        instance_type,
        arch,
        state: instance
            .state
            .as_ref()
            .and_then(|s| s.name.as_ref())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string()),
        public_ip: instance
            .public_ip_address
            .as_deref()
            .unwrap_or("-")
            .to_string(),
        rate,
        cost,
        running_time,
        running_secs,
        vcpu,
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

fn format_core_time(core_secs: u64) -> String {
    let core_hours = core_secs / 3600;
    let core_mins = (core_secs % 3600) / 60;
    if core_hours > 0 {
        format!("{}ch {}cm", core_hours, core_mins)
    } else {
        format!("{}cm", core_mins)
    }
}

/// Terminate all spotty-managed instances that have AutoCleanup=true.
/// Used by the watch daemon on sleep events.
/// Queries all regions that have tracked instances in the local state file.
pub async fn stop_auto_cleanup_instances(_region: Option<String>) -> Result<Vec<String>> {
    // Load state to find which regions have auto-cleanup instances
    let state = crate::state::State::load()?;
    let regions = state.get_auto_cleanup_regions();

    if regions.is_empty() {
        return Ok(Vec::new());
    }

    let mut all_terminated: Vec<String> = Vec::new();

    for region in regions {
        let clients = AwsClients::new(Some(region.clone()))?;

        // Query instances with ManagedBy=spotty AND AutoCleanup=true AND running/pending
        let resp = ec2::describe_instances(
            &clients.client,
            ec2::DescribeInstancesRequest::default()
                .filters(filter_managed_by_spotty())
                .filters(filter_auto_cleanup_true())
                .filters(filter_running_or_pending()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut instance_ids: Vec<String> = Vec::new();

        for reservation in resp.reservations.iter().flat_map(|l| &l.item) {
            for instance in reservation.instances.iter().flat_map(|l| &l.item) {
                if let Some(ref id) = instance.instance_id {
                    instance_ids.push(id.clone());
                }
            }
        }

        if instance_ids.is_empty() {
            continue;
        }

        // Terminate without confirmation (daemon use)
        let req = ec2::TerminateInstancesRequest {
            instance_ids: ec2::InstanceIdStringList::from(instance_ids.clone()),
            ..Default::default()
        };
        ec2::terminate_instances(&clients.client, req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        all_terminated.extend(instance_ids);
    }

    // Untrack all terminated instances
    if !all_terminated.is_empty() {
        if let Err(e) = crate::state::untrack_instances(&all_terminated) {
            eprintln!("Warning: Failed to untrack instances: {}", e);
        }
    }

    Ok(all_terminated)
}

/// Info about a running instance for selection.
#[derive(Debug, Clone)]
pub struct RunningInstance {
    pub instance_id: String,
    pub name: String,
    pub instance_type: String,
    pub public_ip: Option<String>,
}

/// Get list of running spotty-managed instances.
pub async fn get_running_instances(region: Option<String>) -> Result<Vec<RunningInstance>> {
    let clients = AwsClients::new(region)?;

    let resp = ec2::describe_instances(
        &clients.client,
        ec2::DescribeInstancesRequest::default()
            .filters(filter_managed_by_spotty())
            .filters(filter_running_or_pending()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let mut instances: Vec<RunningInstance> = Vec::new();

    for reservation in resp.reservations.iter().flat_map(|l| &l.item) {
        for instance in reservation.instances.iter().flat_map(|l| &l.item) {
            let instance_id = instance.instance_id.as_deref().unwrap_or("-").to_string();
            let name = instance
                .tags
                .as_ref()
                .iter()
                .flat_map(|l| &l.item)
                .find(|t| t.key.as_deref() == Some("Name"))
                .and_then(|t| t.value.as_deref())
                .unwrap_or("-")
                .to_string();
            let instance_type = instance
                .instance_type
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());
            let public_ip = instance.public_ip_address.clone();

            instances.push(RunningInstance {
                instance_id,
                name,
                instance_type,
                public_ip,
            });
        }
    }

    // Sort by name for consistent ordering
    instances.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(instances)
}

/// Resolve instance selector to instance ID.
/// - If selector is a number (1, 2, 3...), select by index
/// - If selector starts with "i-", use as instance ID directly
/// - If selector is None, return first instance or show menu
pub async fn resolve_instance_selector(
    selector: Option<String>,
    region: Option<String>,
) -> Result<Option<String>> {
    resolve_instance_selector_impl(selector, region, true).await
}

pub async fn resolve_instance_selector_no_auto(
    selector: Option<String>,
    region: Option<String>,
) -> Result<Option<String>> {
    resolve_instance_selector_impl(selector, region, false).await
}

async fn resolve_instance_selector_impl(
    selector: Option<String>,
    region: Option<String>,
    auto_select_single: bool,
) -> Result<Option<String>> {
    // If selector looks like an instance ID, return it directly
    if let Some(ref s) = selector {
        if s.starts_with("i-") {
            return Ok(Some(s.clone()));
        }
    }

    // Get running instances
    let instances = get_running_instances(region).await?;

    if instances.is_empty() {
        return Ok(None);
    }

    // If selector is a number, use as index
    if let Some(ref s) = selector {
        if let Ok(idx) = s.parse::<usize>() {
            if idx == 0 || idx > instances.len() {
                bail!("Instance #{} not found (have {} running)", idx, instances.len());
            }
            return Ok(Some(instances[idx - 1].instance_id.clone()));
        }
        // Not a number and not an instance ID - treat as name match?
        // For now, error
        bail!("Invalid instance selector: '{}'. Use a number (1, 2, ...) or instance ID (i-...)", s);
    }

    // No selector - auto-select or show menu
    if auto_select_single && instances.len() == 1 {
        let inst = &instances[0];
        eprintln!("Using instance: {} ({}, {})", inst.name, inst.instance_id, inst.instance_type);
        return Ok(Some(inst.instance_id.clone()));
    }

    // Multiple instances - show menu
    eprintln!("Multiple instances running. Select one:\n");
    for (i, inst) in instances.iter().enumerate() {
        let ip = inst.public_ip.as_deref().unwrap_or("-");
        eprintln!("  {}. {} ({}, {}, {})", i + 1, inst.name, inst.instance_id, inst.instance_type, ip);
    }
    eprintln!();

    print!("Enter number (1-{}): ", instances.len());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let idx: usize = input.trim().parse().context("Invalid number")?;
    if idx == 0 || idx > instances.len() {
        bail!("Invalid selection");
    }

    Ok(Some(instances[idx - 1].instance_id.clone()))
}
