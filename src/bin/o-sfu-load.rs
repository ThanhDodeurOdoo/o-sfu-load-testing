use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use o_sfu_load_testing::{
    ScenarioSpec,
    controller::{RunConfig, run},
};

#[derive(Parser)]
#[command(about = "Run o-sfu and an isolated RTC load generator")]
struct Cli {
    #[arg(long)]
    server_binary: PathBuf,
    #[arg(long)]
    rtc_binary: PathBuf,
    #[arg(long, default_value = "artifacts")]
    output: PathBuf,
    #[arg(long, default_value_t = 1)]
    receivers: u32,
    #[arg(long, default_value_t = 50)]
    packets: u32,
    #[arg(long)]
    server_cpus: Option<String>,
    #[arg(long)]
    rtc_cpus: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let spec = ScenarioSpec::new(cli.receivers, cli.packets)?;
    run(RunConfig {
        server_binary: cli.server_binary,
        rtc_binary: cli.rtc_binary,
        output_directory: cli.output,
        server_cpus: cli.server_cpus,
        rtc_cpus: cli.rtc_cpus,
        spec,
    })
    .await?;
    Ok(())
}
