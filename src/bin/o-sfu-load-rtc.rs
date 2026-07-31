use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use o_sfu_load_testing::{ScenarioSpec, client};

#[derive(Parser)]
#[command(about = "Drive real RTC peers against an o-sfu process")]
struct Cli {
    #[arg(long)]
    base_url: String,
    #[arg(long)]
    websocket_url: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    receivers: u32,
    #[arg(long)]
    packets: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let spec = ScenarioSpec::new(cli.receivers, cli.packets)?;
    Box::pin(client::run(
        &cli.base_url,
        &cli.websocket_url,
        &cli.output,
        spec,
    ))
    .await?;
    Ok(())
}
