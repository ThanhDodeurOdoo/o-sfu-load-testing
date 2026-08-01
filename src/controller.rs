use std::{
    fs::File,
    io::ErrorKind,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use o_sfu::http::route;
#[cfg(not(unix))]
use tokio::signal::ctrl_c;
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::{
    fs,
    process::{Child, Command},
    time::{sleep, timeout},
};

use crate::{
    AUTH_KEY, ScenarioResult, ScenarioSpec,
    profile::ServerProfiler,
    telemetry::{TelemetryConfig, TelemetrySampler},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_ATTEMPTS: u8 = 3;
const READINESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SETUP_ROUND_TIMEOUT: Duration = Duration::from_secs(20);
const RTC_WORKER_OVERHEAD: Duration = Duration::from_secs(40);

pub struct RunConfig {
    pub server_binary: PathBuf,
    pub rtc_binary: PathBuf,
    pub output_directory: PathBuf,
    pub server_cpus: Option<String>,
    pub rtc_cpus: Option<String>,
    pub profile_server: bool,
    pub spec: ScenarioSpec,
}

struct RtcWorkerArtifacts<'a> {
    result: &'a Path,
    spec: &'a Path,
    samples: &'a Path,
}

/// Runs the isolated server and RTC processes then validates their result.
///
/// # Errors
///
/// Returns an error when process startup, readiness, RTC work, result
/// validation, profiling or graceful shutdown fails.
pub async fn run(config: RunConfig) -> Result<ScenarioResult> {
    validate_cpu_set(config.server_cpus.as_deref())?;
    validate_cpu_set(config.rtc_cpus.as_deref())?;
    fs::create_dir_all(&config.output_directory)
        .await
        .context("failed to create the artifact directory")?;

    let result_path = config.output_directory.join("result.json");
    let spec_path = config.output_directory.join("scenario.json");
    let samples_path = config.output_directory.join("samples.jsonl");
    let rtc_stdout_path = config.output_directory.join("rtc.stdout.log");
    let rtc_stderr_path = config.output_directory.join("rtc.stderr.log");
    for path in [
        &result_path,
        &spec_path,
        &samples_path,
        &rtc_stdout_path,
        &rtc_stderr_path,
    ] {
        remove_stale_artifact(path).await?;
    }
    let encoded_spec = serde_json::to_vec_pretty(&config.spec)
        .context("failed to encode the scenario specification")?;
    fs::write(&spec_path, encoded_spec)
        .await
        .context("failed to write scenario.json")?;
    let mut shutdown_signals = ShutdownSignals::new()?;
    let (mut server, base_url, websocket_url) =
        start_server(&config, &mut shutdown_signals).await?;
    let profiler = if config.profile_server {
        let server_pid = server.id().context("o-sfu process has no process id")?;
        match ServerProfiler::start(server_pid, &config.output_directory).await {
            Ok(profiler) => Some(profiler),
            Err(error) => {
                let shutdown_result = stop_server(&mut server).await;
                return combine_run_and_shutdown(
                    Err(error.context("failed to start o-sfu profiling")),
                    shutdown_result,
                );
            }
        }
    } else {
        None
    };
    let mut server_exited = false;
    let run_result = async {
        match run_rtc_worker(
            &config,
            &base_url,
            &websocket_url,
            RtcWorkerArtifacts {
                result: &result_path,
                spec: &spec_path,
                samples: &samples_path,
            },
            &mut server,
            &mut shutdown_signals,
        )
        .await?
        {
            RtcWorkerCompletion::Completed => {}
            RtcWorkerCompletion::ServerExited(status) => {
                server_exited = true;
                return Err(anyhow!("o-sfu exited during RTC work with {status}"));
            }
        }
        let payload = fs::read(&result_path)
            .await
            .context("RTC worker did not write result.json")?;
        let result: ScenarioResult =
            serde_json::from_slice(&payload).context("failed to decode result.json")?;
        result.validate(config.spec)?;
        Ok(result)
    }
    .await;
    let profile_result = match profiler {
        Some(profiler) => profiler.finish().await,
        None => Ok(()),
    };
    let run_result = combine_run_and_profile(run_result, profile_result);
    let shutdown_result = if server_exited {
        Ok(())
    } else {
        stop_server(&mut server).await
    };
    combine_run_and_shutdown(run_result, shutdown_result)
}

fn combine_run_and_profile(
    run_result: Result<ScenarioResult>,
    profile_result: Result<()>,
) -> Result<ScenarioResult> {
    match (run_result, profile_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(run_error), Err(profile_error)) => Err(anyhow!(
            "RTC work failed: {run_error:#}. profile shutdown also failed: {profile_error:#}"
        )),
    }
}

async fn start_server(
    config: &RunConfig,
    shutdown_signals: &mut ShutdownSignals,
) -> Result<(Child, String, String)> {
    for attempt in 1..=STARTUP_ATTEMPTS {
        let http_port = reserve_tcp_port()?;
        let base_url = format!("http://{}:{http_port}", Ipv4Addr::LOCALHOST);
        let websocket_url = format!("ws://{}:{http_port}/", Ipv4Addr::LOCALHOST);
        let mut server = spawn_server(config, http_port)?;
        let readiness_result = tokio::select! {
            result = wait_until_ready(&mut server, &base_url) => result,
            signal_result = shutdown_signals.recv() => {
                let signal_error = signal_result.map_or_else(
                    |error| error,
                    |()| anyhow!("load run interrupted during o-sfu startup"),
                );
                let exited = server
                    .try_wait()
                    .context("failed to inspect o-sfu after a shutdown signal")?
                    .is_some();
                let shutdown_result = if exited {
                    Ok(())
                } else {
                    stop_server(&mut server).await
                };
                return combine_startup_and_shutdown(signal_error, shutdown_result);
            }
        };
        match readiness_result {
            Ok(()) => return Ok((server, base_url, websocket_url)),
            Err(error) => {
                let exited = server
                    .try_wait()
                    .context("failed to inspect o-sfu after startup failure")?
                    .is_some();
                if exited && attempt < STARTUP_ATTEMPTS {
                    continue;
                }
                if exited {
                    return Err(error.context(format!(
                        "o-sfu failed all {STARTUP_ATTEMPTS} startup attempts"
                    )));
                }
                let shutdown_result = stop_server(&mut server).await;
                return combine_startup_and_shutdown(error, shutdown_result);
            }
        }
    }
    Err(anyhow!("o-sfu startup attempts were exhausted"))
}

fn spawn_server(config: &RunConfig, http_port: u16) -> Result<Child> {
    let stdout = File::create(config.output_directory.join("o-sfu.stdout.log"))
        .context("failed to create the o-sfu stdout log")?;
    let stderr = File::create(config.output_directory.join("o-sfu.stderr.log"))
        .context("failed to create the o-sfu stderr log")?;
    let policy = crate::ServerPolicy::for_scenario(config.spec);
    let mut command = isolated_command(&config.server_binary, config.server_cpus.as_deref());
    command
        .env("ANNOUNCED_IP", Ipv4Addr::LOCALHOST.to_string())
        .env("AUTH_KEY", AUTH_KEY)
        .env(
            "BIND_ADDRESS",
            format!("{}:{http_port}", Ipv4Addr::LOCALHOST),
        )
        .env("ROOM_SIZE", policy.room_size.to_string())
        .env("RTC_MEDIA_WORKER_COUNT", policy.media_workers.to_string())
        .env(
            "MAX_PRE_AUTH_WEBSOCKET_SESSIONS_PER_ORIGIN",
            policy
                .max_pre_auth_websocket_sessions_per_origin
                .to_string(),
        )
        .env(
            "ROOM_MAX_ACTIVE_AUDIO_SPEAKERS",
            policy.max_active_audio_speakers.to_string(),
        )
        .env(
            "ROOM_MAX_VIDEO_DOWNLOADS_PER_RECEIVER",
            policy.max_video_downloads_per_receiver.to_string(),
        )
        .env("MAX_BITRATE_IN", policy.max_bitrate_in_bps.to_string())
        .env("MAX_BITRATE_OUT", policy.max_bitrate_out_bps.to_string())
        .env("RTC_MIN_PORT", "49152")
        .env("RTC_MAX_PORT", "65535")
        .env("RTC_UDP_IO_BACKEND", "tokio")
        .env("SHUTDOWN_TIMEOUT_MS", "10000")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.spawn().context("failed to start the o-sfu process")
}

async fn wait_until_ready(server: &mut Child, base_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(READINESS_REQUEST_TIMEOUT)
        .timeout(READINESS_REQUEST_TIMEOUT)
        .build()
        .context("failed to build the readiness HTTP client")?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = server
            .try_wait()
            .context("failed to inspect the o-sfu process")?
        {
            return Err(anyhow!("o-sfu exited before readiness with {status}"));
        }
        if client
            .get(format!("{base_url}{}", route::v1::NOOP))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
    Err(anyhow!("o-sfu did not become ready"))
}

async fn run_rtc_worker(
    config: &RunConfig,
    base_url: &str,
    websocket_url: &str,
    artifacts: RtcWorkerArtifacts<'_>,
    server: &mut Child,
    shutdown_signals: &mut ShutdownSignals,
) -> Result<RtcWorkerCompletion> {
    let stdout = File::create(config.output_directory.join("rtc.stdout.log"))
        .context("failed to create the RTC stdout log")?;
    let stderr = File::create(config.output_directory.join("rtc.stderr.log"))
        .context("failed to create the RTC stderr log")?;
    let mut command = isolated_command(&config.rtc_binary, config.rtc_cpus.as_deref());
    command
        .arg("--base-url")
        .arg(base_url)
        .arg("--websocket-url")
        .arg(websocket_url)
        .arg("--output")
        .arg(artifacts.result)
        .arg("--spec")
        .arg(artifacts.spec)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut rtc = command.spawn().context("failed to start the RTC worker")?;
    let server_pid = server.id().context("o-sfu process has no process id")?;
    let rtc_pid = rtc.id().context("RTC worker has no process id")?;
    let telemetry = TelemetrySampler::start(TelemetryConfig::new(
        base_url,
        server_pid,
        rtc_pid,
        artifacts.samples,
    ))
    .await
    .context("failed to start telemetry sampling")?;
    let deadline = rtc_worker_deadline(config.spec);
    let completion = async {
        tokio::select! {
            status = rtc.wait() => {
                let status = status.context("failed to wait for the RTC worker")?;
                ensure!(status.success(), "RTC worker exited with {status}");
                Ok(RtcWorkerCompletion::Completed)
            }
            status = server.wait() => {
                let status = status.context("failed to wait for o-sfu during RTC work")?;
                stop_rtc_worker(&mut rtc).await?;
                Ok(RtcWorkerCompletion::ServerExited(status))
            }
            () = sleep(deadline) => {
                stop_rtc_worker(&mut rtc).await?;
                Err(anyhow!(
                    "RTC worker exceeded its {} second deadline",
                    deadline.as_secs()
                ))
            }
            signal_result = shutdown_signals.recv() => {
                let signal_error = signal_result.map_or_else(
                    |error| error,
                    |()| anyhow!("load run interrupted during RTC work"),
                );
                stop_rtc_worker(&mut rtc)
                    .await
                    .context("failed to stop the RTC worker after a shutdown signal")?;
                Err(signal_error)
            }
        }
    }
    .await;
    let telemetry_result = telemetry.finish().await;
    match (completion, telemetry_result) {
        (Ok(completion), Ok(_summary)) => Ok(completion),
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
        (Err(run_error), Err(telemetry_error)) => Err(anyhow!(
            "RTC work failed: {run_error:#}. telemetry shutdown also failed: {telemetry_error:#}"
        )),
    }
}

enum RtcWorkerCompletion {
    Completed,
    ServerExited(ExitStatus),
}

fn rtc_worker_deadline(spec: ScenarioSpec) -> Duration {
    let setup_rounds = u64::from(spec.audio_publishers_per_room())
        .saturating_add(u64::from(spec.video_publishers_per_room()))
        .saturating_add(1);
    let setup_seconds = setup_rounds.saturating_mul(SETUP_ROUND_TIMEOUT.as_secs());
    let media_seconds = u64::from(spec.duration_seconds());
    Duration::from_secs(setup_seconds)
        .saturating_add(Duration::from_secs(media_seconds))
        .saturating_add(RTC_WORKER_OVERHEAD)
}

async fn stop_rtc_worker(rtc: &mut Child) -> Result<()> {
    if rtc
        .try_wait()
        .context("failed to inspect the RTC worker")?
        .is_none()
    {
        rtc.kill().await.context("failed to stop the RTC worker")?;
    }
    Ok(())
}

async fn remove_stale_artifact(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove stale {}", path.display()))
        }
    }
}

async fn stop_server(server: &mut Child) -> Result<()> {
    if server
        .try_wait()
        .context("failed to inspect the o-sfu process")?
        .is_some()
    {
        return Err(anyhow!("o-sfu exited before shutdown"));
    }
    let process_id = server.id().context("o-sfu process has no process id")?;
    let signal_status = Command::new("kill")
        .arg("-TERM")
        .arg(process_id.to_string())
        .status()
        .await
        .context("failed to signal the o-sfu process")?;
    ensure!(signal_status.success(), "failed to send SIGTERM to o-sfu");
    match timeout(SHUTDOWN_TIMEOUT, server.wait()).await {
        Ok(status) => {
            let status = status.context("failed to wait for o-sfu shutdown")?;
            ensure!(status.success(), "o-sfu shutdown exited with {status}");
            Ok(())
        }
        Err(_elapsed) => {
            server.kill().await.context("failed to force-stop o-sfu")?;
            Err(anyhow!("o-sfu exceeded its shutdown deadline"))
        }
    }
}

fn isolated_command(binary: &Path, cpu_set: Option<&str>) -> Command {
    cpu_set.map_or_else(
        || Command::new(binary),
        |cpu_set| {
            let mut command = Command::new("taskset");
            command.arg("--cpu-list").arg(cpu_set).arg(binary);
            command
        },
    )
}

fn reserve_tcp_port() -> Result<u16> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("failed to reserve an HTTP port")?;
    listener
        .local_addr()
        .map(|address| address.port())
        .context("failed to read the reserved HTTP port")
}

fn validate_cpu_set(cpu_set: Option<&str>) -> Result<()> {
    let Some(cpu_set) = cpu_set else {
        return Ok(());
    };
    ensure!(!cpu_set.is_empty(), "CPU set cannot be empty");
    ensure!(
        cpu_set
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b',' | b'-')),
        "CPU set must contain CPU numbers, commas and ranges"
    );
    Ok(())
}

fn combine_run_and_shutdown(
    run_result: Result<ScenarioResult>,
    shutdown_result: Result<()>,
) -> Result<ScenarioResult> {
    match (run_result, shutdown_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(run_error), Err(shutdown_error)) => Err(anyhow!(
            "load run failed: {run_error:#}. o-sfu shutdown also failed: {shutdown_error:#}"
        )),
    }
}

fn combine_startup_and_shutdown(
    startup_error: anyhow::Error,
    shutdown_result: Result<()>,
) -> Result<(Child, String, String)> {
    match shutdown_result {
        Ok(()) => Err(startup_error),
        Err(shutdown_error) => Err(anyhow!(
            "o-sfu startup failed: {startup_error:#}. shutdown also failed: {shutdown_error:#}"
        )),
    }
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: Signal,
    terminate: Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())
                .context("failed to install the SIGINT handler")?,
            terminate: signal(SignalKind::terminate())
                .context("failed to install the SIGTERM handler")?,
        })
    }

    async fn recv(&mut self) -> Result<()> {
        let received = tokio::select! {
            received = self.interrupt.recv() => received,
            received = self.terminate.recv() => received,
        };
        ensure!(received.is_some(), "shutdown signal stream closed");
        Ok(())
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> Result<()> {
        ctrl_c().await.context("failed to wait for Ctrl-C")
    }
}

#[cfg(test)]
#[path = "TESTS/controller_tests.rs"]
mod tests;
