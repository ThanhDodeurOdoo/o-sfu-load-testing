use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
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
    #[arg(long)]
    server_cpus: Option<String>,
    #[arg(long)]
    rtc_cpus: Option<String>,
    #[arg(long)]
    profile_server: bool,
    #[command(subcommand)]
    scenario: ScenarioCommand,
}

#[derive(Subcommand)]
enum ScenarioCommand {
    Smoke {
        #[arg(long, default_value_t = 1)]
        receivers: u32,
        #[arg(long, default_value_t = 50)]
        packets: u32,
    },
    AudioMesh {
        #[arg(long)]
        rooms: u32,
        #[arg(long)]
        peers: u32,
        #[arg(long)]
        seconds: u32,
    },
    VideoGallery {
        #[arg(long)]
        rooms: u32,
        #[arg(long)]
        peers: u32,
        #[arg(long)]
        publishers: u32,
        #[arg(long)]
        seconds: u32,
    },
    MixedConference {
        #[arg(long)]
        rooms: u32,
        #[arg(long)]
        peers: u32,
        #[arg(long)]
        audio_publishers: u32,
        #[arg(long)]
        video_publishers: u32,
        #[arg(long)]
        seconds: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let spec = match cli.scenario {
        ScenarioCommand::Smoke { receivers, packets } => ScenarioSpec::smoke(receivers, packets)?,
        ScenarioCommand::AudioMesh {
            rooms,
            peers,
            seconds,
        } => ScenarioSpec::audio_mesh(rooms, peers, seconds)?,
        ScenarioCommand::VideoGallery {
            rooms,
            peers,
            publishers,
            seconds,
        } => ScenarioSpec::video_gallery(rooms, peers, publishers, seconds)?,
        ScenarioCommand::MixedConference {
            rooms,
            peers,
            audio_publishers,
            video_publishers,
            seconds,
        } => ScenarioSpec::mixed_conference(
            rooms,
            peers,
            audio_publishers,
            video_publishers,
            seconds,
        )?,
    };
    run(RunConfig {
        server_binary: cli.server_binary,
        rtc_binary: cli.rtc_binary,
        output_directory: cli.output,
        server_cpus: cli.server_cpus,
        rtc_cpus: cli.rtc_cpus,
        profile_server: cli.profile_server,
        spec,
    })
    .await?;
    Ok(())
}
