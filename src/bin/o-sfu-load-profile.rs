use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use o_sfu_load_testing::profile;

#[derive(Parser)]
#[command(about = "Prepare and report an o-sfu CPU profile")]
struct Cli {
    #[command(subcommand)]
    command: ProfileCommand,
}

#[derive(Subcommand)]
enum ProfileCommand {
    Prepare {
        #[arg(long)]
        input: PathBuf,
    },
    Report {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        artifact_url: Option<String>,
        #[arg(long)]
        flamegraph_url: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        ProfileCommand::Prepare { input } => profile::prepare(&input),
        ProfileCommand::Report {
            input,
            output,
            artifact_url,
            flamegraph_url,
        } => profile::write(
            &input,
            &output,
            artifact_url.as_deref(),
            flamegraph_url.as_deref(),
        ),
    }
}
