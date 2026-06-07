use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(
    name = "ilink-relay",
    about = "Public pairing relay for iLink Hub (zero-config phone binding)"
)]
struct Cli {
    /// Listen address
    #[arg(long, default_value = "127.0.0.1:8789", env = "ILINK_RELAY_ADDR")]
    addr: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ilink_hub=info".parse()?)
                .add_directive("ilink_relay=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    info!(addr = %cli.addr, "starting ilink-relay");
    ilink_hub::relay::server::serve(&cli.addr).await
}
