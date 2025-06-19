use clap::{Parser, Subcommand};
use tracing::{info, error};
use tracing_subscriber;
use anyhow::Result;

mod proxy;
mod config;
mod ai;

use proxy::ProxyServer;
use config::ProxyConfig;

#[derive(Parser)]
#[command(name = "sidecar-proxy")]
#[command(about = "High-performance Rust sidecar proxy for autonomous service mesh operations")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Start {
        #[arg(short, long, default_value = "proxy.yaml")]
        config: String,
        #[arg(long)]
        ai_enabled: bool,
    },
    Validate {
        #[arg(short, long, default_value = "proxy.yaml")]
        config: String,
    },
    GenerateConfig {
        #[arg(short, long, default_value = "proxy.yaml")]
        output: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Start { config, ai_enabled } => {
            info!("Starting sidecar proxy with config: {}", config);
            let proxy_config = ProxyConfig::load(&config)?;
            let mut server = ProxyServer::new(proxy_config, ai_enabled).await?;
            server.run().await?;
        }
        Commands::Validate { config } => {
            info!("Validating configuration: {}", config);
            match ProxyConfig::load(&config) {
                Ok(_) => info!("Configuration is valid"),
                Err(e) => {
                    error!("Configuration validation failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::GenerateConfig { output } => {
            info!("Generating sample configuration: {}", output);
            ProxyConfig::generate_sample(&output)?;
            info!("Sample configuration generated successfully");
        }
    }
    
    Ok(())
}
