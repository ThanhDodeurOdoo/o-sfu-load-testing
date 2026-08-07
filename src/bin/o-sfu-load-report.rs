use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use clap::Parser;
use o_sfu_load_testing::{comparison, report, report::DashboardConfig};

#[derive(Parser)]
#[command(about = "Render o-sfu load results as a GitHub job summary")]
struct Cli {
    /// Result files or directories for one o-sfu revision.
    #[arg(long)]
    input: Vec<PathBuf>,
    /// Result files or directories for the baseline revision.
    #[arg(long)]
    baseline_input: Vec<PathBuf>,
    /// Result files or directories for the comparison revision.
    #[arg(long)]
    comparison_input: Vec<PathBuf>,
    /// Markdown report destination.
    #[arg(long)]
    output: PathBuf,
    /// GitHub Actions artifact URL used for deep-investigation links.
    #[arg(long)]
    artifact_url: Option<String>,
    /// Directory that receives deterministic Plotters SVG dashboards.
    #[arg(long)]
    dashboard_output: Option<PathBuf>,
    /// Safe filename prefix paired with `--dashboard-output`.
    #[arg(long)]
    dashboard_asset_stem: Option<String>,
    /// Public load-test-assets release URL used for embedded PNG previews.
    #[arg(long)]
    dashboard_url_base: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        cli.dashboard_output.is_some() == cli.dashboard_asset_stem.is_some(),
        "--dashboard-output and --dashboard-asset-stem must be used together"
    );
    ensure!(
        cli.dashboard_url_base.is_none() || cli.dashboard_output.is_some(),
        "--dashboard-url-base requires --dashboard-output"
    );
    let dashboards = cli
        .dashboard_output
        .as_deref()
        .zip(cli.dashboard_asset_stem.as_deref())
        .map(|(output_directory, asset_stem)| DashboardConfig {
            output_directory,
            asset_stem,
            public_url_base: cli.dashboard_url_base.as_deref(),
        });
    match (
        cli.input.is_empty(),
        cli.baseline_input.is_empty(),
        cli.comparison_input.is_empty(),
    ) {
        (false, true, true) => report::write(
            &cli.input,
            &cli.output,
            cli.artifact_url.as_deref(),
            dashboards.as_ref(),
        ),
        (true, false, false) => comparison::write(
            &cli.baseline_input,
            &cli.comparison_input,
            &cli.output,
            cli.artifact_url.as_deref(),
            dashboards.as_ref(),
        ),
        _ => bail!(
            "use either --input or matching --baseline-input and --comparison-input arguments"
        ),
    }
}
