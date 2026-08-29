use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use tabled::Tabled;
use tracing::{debug, instrument};

use super::client::AwsClients;
use super::filters::{get_instance_types, InstanceTypeFilter};
use super::instances::fetch_all_spot_prices;
use crate::ec2;
use crate::SortField;

#[derive(Debug, Serialize, Tabled)]
pub struct SpotPrice {
    #[tabled(rename = "Instance Type")]
    pub instance_type: String,
    #[tabled(rename = "vCPU")]
    pub vcpu: i32,
    #[tabled(rename = "Memory (GB)")]
    #[serde(rename = "memory_gb")]
    pub memory_gb: String,
    #[tabled(rename = "Arch")]
    pub arch: String,
    #[tabled(rename = "AZ")]
    pub availability_zone: String,
    #[tabled(rename = "Price/hr")]
    pub price: String,
    #[tabled(rename = "¢/core/hr")]
    #[serde(rename = "cents_per_core_hr")]
    pub price_per_core: String,
}

#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(region, instance_type))]
pub async fn list_spot_prices(
    region: Option<String>,
    instance_type: Option<String>,
    vcpu_min: Option<i32>,
    vcpu_max: Option<i32>,
    memory_min: Option<f64>,
    memory_max: Option<f64>,
    arch: Option<&str>,
    max_price: Option<f64>,
    sort: SortField,
    limit: usize,
) -> Result<Vec<SpotPrice>> {
    debug!("Creating AWS clients");
    let clients = AwsClients::new(region).await?;
    debug!(region = %clients.region, "Using region");

    debug!("Fetching instance types and spot prices concurrently");

    // If a specific instance type is requested, fetch only that
    let prices = if let Some(ref it) = instance_type {
        debug!("Fetching prices for specific instance type: {}", it);
        fetch_spot_prices_for_type(&clients, it).await?
    } else {
        debug!("Fetching all spot prices");
        let (instance_types_result, prices_result) = tokio::join!(
            get_instance_types(&clients),
            fetch_all_spot_prices(&clients)
        );
        let _ = instance_types_result?; // Pre-fetch for filtering
        prices_result?
    };

    let instance_types = get_instance_types(&clients).await?;
    debug!(count = instance_types.len(), "Fetched instance types");
    debug!(count = prices.len(), "Fetched spot prices");

    let filter = InstanceTypeFilter {
        vcpu_min,
        vcpu_max,
        memory_min_gb: memory_min,
        memory_max_gb: memory_max,
        arch: arch.map(String::from),
    };

    let mut results: Vec<SpotPrice> = prices
        .into_iter()
        .filter_map(|(it, az, price)| {
            // If specific instance type requested, skip filter check for type matching
            if let Some(ref requested_type) = instance_type {
                if &it != requested_type {
                    return None;
                }
            }

            let spec = instance_types.get(&it)?;

            if !filter.matches(spec) {
                return None;
            }

            if let Some(max) = max_price {
                if price > max {
                    return None;
                }
            }

            let cents_per_core = (price / spec.vcpu as f64) * 100.0;
            Some(SpotPrice {
                instance_type: it,
                vcpu: spec.vcpu,
                memory_gb: format!("{:.1}", spec.memory_gb()),
                arch: spec.architectures.first().cloned().unwrap_or_default(),
                availability_zone: az,
                price: format!("${:.4}", price),
                price_per_core: format!("{:.2}", cents_per_core),
            })
        })
        .collect();

    // Sort results
    match sort {
        SortField::Price => {
            results.sort_by(|a, b| {
                let pa: f64 = a.price.trim_start_matches('$').parse().unwrap_or(0.0);
                let pb: f64 = b.price.trim_start_matches('$').parse().unwrap_or(0.0);
                pa.partial_cmp(&pb).unwrap()
            });
        }
        SortField::Vcpu => {
            results.sort_by_key(|r| r.vcpu);
        }
        SortField::Memory => {
            results.sort_by(|a, b| {
                let ma: f64 = a.memory_gb.parse().unwrap_or(0.0);
                let mb: f64 = b.memory_gb.parse().unwrap_or(0.0);
                ma.partial_cmp(&mb).unwrap()
            });
        }
    }

    results.truncate(limit);

    Ok(results)
}

/// Fetch spot prices for a specific instance type (optimized single-type query)
async fn fetch_spot_prices_for_type(
    clients: &AwsClients,
    instance_type: &str,
) -> Result<Vec<(String, String, f64)>> {
    let mut prices = Vec::new();

    let it_enum = ec2::InstanceType::from(instance_type);

    let resp = ec2::describe_spot_price_history(
        &clients.client,
        ec2::DescribeSpotPriceHistoryRequest::default()
            .start_time(Utc::now().to_rfc3339())
            .filters(
                ec2::Filter::default()
                    .name("product-description")
                    .values("Linux/UNIX"),
            )
            .instance_types(it_enum),
    )
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

    // Deduplicate - keep lowest price per instance type + AZ
    let mut best_prices: std::collections::HashMap<(String, String), f64> =
        std::collections::HashMap::new();
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
