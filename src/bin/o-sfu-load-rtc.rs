use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use o_sfu_load_testing::{
    ScenarioSpec, client,
    phase::{PhaseReporter, ScenarioPhase},
};
use tokio::fs;

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
    spec: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let payload = fs::read(&cli.spec).await?;
    let spec: ScenarioSpec = serde_json::from_slice(&payload)?;
    let phases = PhaseReporter::stdio();
    phases.report(ScenarioPhase::Setup)?;
    Box::pin(client::run(
        &cli.base_url,
        &cli.websocket_url,
        &cli.output,
        spec,
        phases,
    ))
    .await?;
    Ok(())
}
