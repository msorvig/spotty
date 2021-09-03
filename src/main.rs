use itertools::zip;

//use aws_sdk_ec2::Region;
use futures::future::join_all;

/// Prints (some) instance type info
fn print_instance_type_info(info: &aws_sdk_ec2::model::InstanceTypeInfo) {
    let instance_type = info.instance_type.as_ref().unwrap().as_str().to_string();
    let cpu_info = info.v_cpu_info.as_ref().unwrap();
    let vcpus = cpu_info.default_v_cpus.unwrap();
    let cores = cpu_info.default_cores.unwrap();
    let processor_info = info.processor_info.as_ref().unwrap();
    let freq = processor_info.sustained_clock_speed_in_ghz.unwrap();
    let arch = processor_info
        .supported_architectures
        .as_ref()
        .unwrap()
        .iter()
        .map(|a| a.as_str().to_string())
        .collect::<String>();
    let mem = info.memory_info.as_ref().unwrap().size_in_mi_b.unwrap();
    println!(
        "e: {:15} {:3} vcpu {:3} core {:4} Ghz {:6} {:3} GB",
        instance_type,
        vcpus,
        cores,
        freq,
        arch,
        mem / 1024
    );
}

/// Create  a client for the given region
fn create_client<T: Into<String>>(region: T) -> aws_sdk_ec2::Client {
    aws_sdk_ec2::Client::from_conf(
        aws_sdk_ec2::Config::builder()
            .region(aws_sdk_ec2::Region::new(region.into()))
            .build(),
    )
}

/// Requests spot price history for multiple regions
async fn request_spot_price_history(
    regions: Vec<String>,
) -> Vec<(String, aws_sdk_ec2::output::DescribeSpotPriceHistoryOutput)> {
    // Make spot price history request
    let result_fut = regions
        .iter()
        .map(create_client)
        .map(|client| {
            client
                .describe_spot_price_history()
                .product_descriptions("Linux/UNIX")
                .instance_types("c5a.16xlarge")
                .instance_types("m6g.metal")
                .max_results(10)
                .send()
        })
        .collect::<Vec<_>>();

    // Wait for response
    let results = join_all(result_fut).await;

    // Map regions to price results
    let region_results = zip(regions.into_iter(), results.into_iter())
        .map(|(region, result)| (region, result.unwrap()))
        .collect::<Vec<_>>();

    region_results
}

/// Calls describe_instance_types() repeatedly using next_token.
async fn describe_instance_types_all<'a>(
    client: &aws_sdk_ec2::Client,
) -> Result<
    Vec<aws_sdk_ec2::model::InstanceTypeInfo>,
    aws_sdk_ec2::SdkError<aws_sdk_ec2::error::DescribeInstanceTypesError>,
>
{
    let mut next_token = None;
    let mut instance_info = Vec::new();
    loop {
        let output = client
            .describe_instance_types()
            .set_next_token(next_token)
            .send()
            .await?;
        match output.instance_types {
            Some(mut instance_types) => {
                instance_info.append(&mut instance_types);
            }
            None => {}
        }
        next_token = output.next_token;
        if next_token.is_none() {
            break;
        }
    }
    Ok(instance_info)
}

async fn request_instance_types(
    regions: Vec<String>,
) -> Vec<(String, Vec<aws_sdk_ec2::model::InstanceTypeInfo>)> {
    let result_fut = regions
        .iter()
        .map(create_client)
        .map(|client| { describe_instance_types_all(&client) })
        .collect::<Vec<_>>();

    let results = join_all(result_fut).await;

    zip(regions.into_iter(), results.into_iter())
        .map(|(region, result)| (region, result.unwrap()))
        .collect::<Vec<_>>()
}

#[tokio::main]
async fn main() -> Result<(), aws_sdk_ec2::Error> {
    let client = aws_sdk_ec2::Client::from_env();

    let rsp = client.describe_regions().send().await?;
    /*
        for region in rsp.regions.as_ref().unwrap() {
            println!("region: {:#?}", region.region_name.as_ref().unwrap());
        }
    */
    let eu_regions = rsp
        .clone()
        .regions
        .unwrap()
        .into_iter()
        .filter(|r| r.region_name.as_ref().unwrap().starts_with("eu"))
        .map(|r| r.region_name.unwrap())
        .collect::<Vec<_>>();
    println!("eu regions: {:?}", eu_regions);

    let all_regions = rsp
        .clone()
        .regions
        .unwrap()
        .into_iter()
        .map(|r| r.region_name.unwrap())
        .collect::<Vec<_>>();

    let types = client.describe_instance_types().send().await?;
    types
        .instance_types
        .unwrap()
        .into_iter()
        .filter(|e| true)
        .take(10)
        .for_each(|info| {
            //  println!("{:?}", &e);
            print_instance_type_info(&info);
        });

    let region_types = request_instance_types(all_regions.clone()).await;

    for (region, result) in region_types {
        println!("t {} {} ", region, result.instance_types.unwrap().len())
    }

    let region_results = request_spot_price_history(all_regions.clone()).await;

    for (region, result) in region_results {
        for price in result.spot_price_history.unwrap() {
            println!(
                "x {} {:?} {:?}",
                region, price.instance_type, price.spot_price
            );
        }
    }

    Ok(())
}
