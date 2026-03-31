use anyhow::Result;
use clap::{Parser, Subcommand};
use spotty::aws::instances::LaunchConfig;
use spotty::{aws, config, keys, launchd, ssh, watch, InstanceFilterArgs, SortField};

#[derive(Parser)]
#[command(name = "spotty")]
#[command(about = "AWS EC2 Spot Instance CLI", long_about = None)]
struct Cli {
    /// Enable verbose tracing output
    #[arg(long, short = 't')]
    trace: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List current spot prices with filtering
    Prices {
        /// AWS region
        #[arg(long, short)]
        region: Option<String>,

        #[command(flatten)]
        filter: InstanceFilterArgs,

        /// Sort by: price, vcpu, memory
        #[arg(long, short, default_value = "price")]
        sort: SortField,

        /// Limit number of results
        #[arg(long, short, default_value = "20")]
        limit: usize,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Launch a spot instance (tries cheapest matching instance until one starts)
    Start {
        #[command(flatten)]
        filter: InstanceFilterArgs,

        /// AMI ID (default: latest Amazon Linux 2023)
        #[arg(long)]
        ami: Option<String>,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,

        /// SSH key pair name in AWS
        #[arg(long, short)]
        key_name: Option<String>,

        /// Security group ID
        #[arg(long, short = 'g')]
        security_group: Option<String>,

        /// Instance name tag
        #[arg(long)]
        name: Option<String>,

        /// Number of instances to start
        #[arg(long, short, default_value = "1")]
        number: usize,

        /// Path to user-data script
        #[arg(long)]
        user_data: Option<String>,

        /// Wait for instances to be running
        #[arg(long, short)]
        wait: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Don't auto-cleanup this instance on sleep
        #[arg(long)]
        no_auto_cleanup: bool,

        /// Maximum number of instance types to try
        #[arg(long, default_value = "10")]
        max_tries: usize,

        /// Root volume size in GB (default: AMI default, typically 2 GB for minimal, 8 GB for full)
        #[arg(long)]
        volume_size: Option<i32>,
    },

    /// Terminate spot instances
    Stop {
        /// Instance IDs to terminate (or use --all)
        instance_ids: Vec<String>,

        /// Terminate all spotty-managed instances
        #[arg(long, short)]
        all: bool,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },

    /// Show instance status
    Status {
        /// Instance IDs (show all spotty instances if not specified)
        instance_ids: Vec<String>,

        /// Show all instances including stopped/terminated
        #[arg(long, short)]
        all: bool,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// SSH into an instance (starts one if needed)
    Ssh {
        /// Instance ID (if not provided, starts a new instance)
        instance_id: Option<String>,

        #[command(flatten)]
        filter: InstanceFilterArgs,

        /// Path to SSH private key
        #[arg(long, short)]
        key: Option<String>,

        /// SSH user (default: ec2-user)
        #[arg(long, short, default_value = "ec2-user")]
        user: String,

        /// Run command instead of interactive shell
        #[arg(long, short)]
        command: Option<String>,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,

        /// Don't auto-cleanup this instance on sleep
        #[arg(long)]
        no_auto_cleanup: bool,
    },

    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage SSH key pairs
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },

    /// Manage the auto-cleanup watcher daemon
    Watch {
        #[command(subcommand)]
        action: Option<WatchAction>,
    },

    /// Manage AMI images (list base AMIs, create custom images)
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set {
        /// Configuration key
        key: String,
        /// Configuration value
        value: String,
    },
    /// Create default configuration file
    Init,
}

#[derive(Subcommand)]
enum KeysAction {
    /// Create a new SSH key pair
    Create {
        /// Key pair name
        name: String,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
    /// List available key pairs
    List {
        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
}

#[derive(Subcommand)]
enum ImageAction {
    /// List available AMIs (base AL2023 + custom from config)
    List {
        /// AWS region
        #[arg(long, short)]
        region: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Create an AMI from a running instance
    Create {
        /// Instance ID (auto-selects if not provided)
        instance_id: Option<String>,

        /// AMI name (auto-generated if not provided)
        #[arg(long)]
        name: Option<String>,

        /// Don't reboot the instance before creating the image
        #[arg(long)]
        no_reboot: bool,

        /// Wait for the AMI to become available
        #[arg(long, short)]
        wait: bool,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
    /// Deregister an AMI and delete its backing snapshots
    Rm {
        /// AMI ID to remove
        ami_id: String,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
    /// Build an AMI: launch instance, run script, bake image, terminate
    Build {
        /// Path to build script
        #[arg(long, short)]
        script: String,

        #[command(flatten)]
        filter: InstanceFilterArgs,

        /// AMI name (auto-generated if not provided)
        #[arg(long)]
        name: Option<String>,

        /// Root volume size in GB (default: 20)
        #[arg(long)]
        volume_size: Option<i32>,

        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
}

#[derive(Subcommand)]
enum WatchAction {
    /// Install the launchd agent (auto-start on login)
    Install,
    /// Uninstall the launchd agent
    Uninstall,
    /// Show watcher status
    Status,
    /// Print the watcher log
    Log,
    /// Set up IAM user with minimal permissions for the watcher
    Setup {
        /// AWS region
        #[arg(long, short)]
        region: Option<String>,
    },
}


#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing if --trace flag is set
    if cli.trace {
        tracing_subscriber::fmt()
            .with_env_filter("spotty=debug")
            .with_target(true)
            .init();
        tracing::debug!("Tracing enabled");
    }

    if let Err(e) = run(cli).await {
        // Check for AWS credential errors by examining the full error chain
        let is_credential_error = {
            // Check Display and Debug output for credential-related keywords
            let display_str = e.to_string().to_lowercase();
            let debug_str = format!("{:?}", e).to_lowercase();

            let keywords = ["credentials", "unauthorizedexception", "expiredtoken", "no credentials"];
            keywords.iter().any(|kw| display_str.contains(kw) || debug_str.contains(kw))
        };

        if is_credential_error {
            eprintln!("Error: AWS credentials are missing or expired.\n");
            eprintln!("If using AWS SSO, run:");
            eprintln!("  aws sso login --profile <your-profile>\n");
            eprintln!("Or set AWS_PROFILE if needed:");
            eprintln!("  export AWS_PROFILE=<your-profile>");
            std::process::exit(1);
        }
        return Err(e);
    }

    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Prices {
            region,
            filter,
            sort,
            limit,
            json,
        } => {
            let arch = filter.arch_str().map(String::from);
            aws::pricing::list_spot_prices(
                region,
                filter.instance_type,
                filter.vcpu_min,
                filter.vcpu_max,
                filter.memory_min,
                filter.memory_max,
                arch.as_deref(),
                filter.max_price,
                sort,
                limit,
                json,
            )
            .await?;
        }
        Commands::Start {
            filter,
            ami,
            region,
            key_name,
            security_group,
            name,
            number,
            user_data,
            wait,
            json,
            no_auto_cleanup,
            max_tries,
            volume_size,
        } => {
            // Auto-install the watcher agent on first start
            if !no_auto_cleanup {
                let _ = launchd::ensure_installed();
            }

            // Get arch before moving filter fields
            let arch = filter.arch_str().map(String::from);
            let config = LaunchConfig {
                instance_type: filter.instance_type,
                vcpu_min: filter.vcpu_min,
                vcpu_max: filter.vcpu_max,
                memory_min: filter.memory_min,
                memory_max: filter.memory_max,
                arch,
                max_price: filter.max_price,
                ami,
                region,
                key_name,
                security_group,
                name,
                user_data,
                no_auto_cleanup,
                max_tries,
                volume_size,
            };
            aws::instances::start_instances(config, number, wait, json).await?;
        }
        Commands::Stop {
            instance_ids,
            all,
            region,
        } => {
            aws::instances::stop_instances(instance_ids, all, region).await?;
        }
        Commands::Status {
            instance_ids,
            all,
            region,
            json,
        } => {
            aws::instances::instance_status(instance_ids, all, region, json).await?;
        }
        Commands::Ssh {
            instance_id,
            filter,
            key,
            user,
            command,
            region,
            no_auto_cleanup,
        } => {
            // Try to resolve instance by selector (number, ID, or auto-select)
            let resolved = aws::instances::resolve_instance_selector(instance_id.clone(), region.clone()).await?;

            let target_instance_id = if let Some(id) = resolved {
                id
            } else {
                // No running instances - need to start one
                if !filter.has_any_filter() {
                    anyhow::bail!("No running instances. Provide filters (--vcpu-min, --memory-min, --arch, etc.) to start a new instance");
                }

                // Auto-install watcher
                if !no_auto_cleanup {
                    let _ = launchd::ensure_installed();
                }

                // Start instance and get its ID
                let arch = filter.arch_str().map(String::from);
                let config = LaunchConfig {
                    instance_type: filter.instance_type,
                    vcpu_min: filter.vcpu_min,
                    vcpu_max: filter.vcpu_max,
                    memory_min: filter.memory_min,
                    memory_max: filter.memory_max,
                    arch,
                    max_price: filter.max_price,
                    region: region.clone(),
                    no_auto_cleanup,
                    max_tries: 10,
                    ..Default::default()
                };
                aws::instances::start_instance_and_get_id(config).await?
            };

            ssh::ssh_into_instance(target_instance_id, key, user, command, region).await?;
        }
        Commands::Config { action } => match action {
            ConfigAction::Show => config::show_config()?,
            ConfigAction::Set { key, value } => config::set_config(&key, &value)?,
            ConfigAction::Init => config::init_config()?,
        },
        Commands::Keys { action } => match action {
            KeysAction::Create { name, region } => keys::create_key_pair(&name, region).await?,
            KeysAction::List { region } => keys::list_key_pairs(region).await?,
        },
        Commands::Image { action } => match action {
            ImageAction::List { region, json } => {
                aws::images::list_images(region, json).await?;
            }
            ImageAction::Create {
                instance_id,
                name,
                no_reboot,
                wait,
                region,
            } => {
                let resolved =
                    aws::instances::resolve_instance_selector(instance_id, region.clone()).await?;
                let id = resolved.ok_or_else(|| {
                    anyhow::anyhow!("No running instances found. Provide an instance ID.")
                })?;
                aws::images::create_image(id, name, no_reboot, wait, region).await?;
            }
            ImageAction::Rm { ami_id, region } => {
                aws::images::remove_image(ami_id, region).await?;
            }
            ImageAction::Build {
                script,
                filter,
                name,
                volume_size,
                region,
            } => {
                let arch = filter.arch_str().map(String::from);
                let launch_config = LaunchConfig {
                    instance_type: filter.instance_type,
                    vcpu_min: filter.vcpu_min,
                    vcpu_max: filter.vcpu_max,
                    memory_min: filter.memory_min,
                    memory_max: filter.memory_max,
                    arch,
                    max_price: filter.max_price,
                    volume_size,
                    no_auto_cleanup: true, // don't auto-cleanup builder instances
                    max_tries: 10,
                    ..Default::default()
                };
                aws::images::build_image(script, name, volume_size, region, launch_config).await?;
            }
        },
        Commands::Watch { action } => match action {
            Some(WatchAction::Install) => launchd::install()?,
            Some(WatchAction::Uninstall) => launchd::uninstall()?,
            Some(WatchAction::Status) => launchd::status()?,
            Some(WatchAction::Log) => launchd::log()?,
            Some(WatchAction::Setup { region }) => launchd::setup(region).await?,
            None => {
                // Run the watcher in foreground (for debugging or manual use)
                watch::run_watch().await?;
            }
        },
    }

    Ok(())
}
