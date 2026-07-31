use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use o_sfu_load_testing::report;

#[derive(Parser)]
#[command(about = "Render o-sfu load results as a GitHub job summary")]
struct Cli {
    #[arg(long, required = true)]
    input: Vec<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    artifact_url: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    report::write(&cli.input, &cli.output, cli.artifact_url.as_deref())
}
