use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use o_sfu_load_testing::{comparison, report};

#[derive(Parser)]
#[command(about = "Render o-sfu load results as a GitHub job summary")]
struct Cli {
    #[arg(long)]
    input: Vec<PathBuf>,
    #[arg(long)]
    baseline_input: Vec<PathBuf>,
    #[arg(long)]
    comparison_input: Vec<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    artifact_url: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match (
        cli.input.is_empty(),
        cli.baseline_input.is_empty(),
        cli.comparison_input.is_empty(),
    ) {
        (false, true, true) => report::write(&cli.input, &cli.output, cli.artifact_url.as_deref()),
        (true, false, false) => comparison::write(
            &cli.baseline_input,
            &cli.comparison_input,
            &cli.output,
            cli.artifact_url.as_deref(),
        ),
        _ => bail!(
            "use either --input or matching --baseline-input and --comparison-input arguments"
        ),
    }
}
