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

use crate::{AUTH_KEY, ScenarioResult, ScenarioSpec};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_ATTEMPTS: u8 = 3;
const READINESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PEER_SETUP_TIMEOUT: Duration = Duration::from_secs(20);
const RTC_WORKER_OVERHEAD: Duration = Duration::from_secs(40);

pub struct RunConfig {
    pub server_binary: PathBuf,
    pub rtc_binary: PathBuf,
    pub output_directory: PathBuf,
    pub server_cpus: Option<String>,
    pub rtc_cpus: Option<String>,
    pub spec: ScenarioSpec,
}

/// Runs the isolated server and RTC processes then validates their result.
///
/// # Errors
///
/// Returns an error when process startup, readiness, RTC work, result
/// validation or graceful shutdown fails.
pub async fn run(config: RunConfig) -> Result<ScenarioResult> {
    validate_cpu_set(config.server_cpus.as_deref())?;
    validate_cpu_set(config.rtc_cpus.as_deref())?;
    fs::create_dir_all(&config.output_directory)
        .await
        .context("failed to create the artifact directory")?;

    let result_path = config.output_directory.join("result.json");
    remove_stale_result(&result_path).await?;
    let mut shutdown_signals = ShutdownSignals::new()?;
    let (mut server, base_url, websocket_url) =
        start_server(&config, &mut shutdown_signals).await?;
    let mut server_exited = false;
    let run_result = async {
        match run_rtc_worker(
            &config,
            &base_url,
            &websocket_url,
            &result_path,
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
    let shutdown_result = if server_exited {
        Ok(())
    } else {
        stop_server(&mut server).await
    };
    combine_run_and_shutdown(run_result, shutdown_result)
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
    let room_size = config.spec.receivers() + 1;
    let mut command = isolated_command(&config.server_binary, config.server_cpus.as_deref());
    command
        .env("ANNOUNCED_IP", Ipv4Addr::LOCALHOST.to_string())
        .env("AUTH_KEY", AUTH_KEY)
        .env(
            "BIND_ADDRESS",
            format!("{}:{http_port}", Ipv4Addr::LOCALHOST),
        )
        .env("ROOM_SIZE", room_size.to_string())
        .env("RTC_MEDIA_WORKER_COUNT", "1")
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
    result_path: &Path,
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
        .arg(result_path)
        .arg("--receivers")
        .arg(config.spec.receivers().to_string())
        .arg("--packets")
        .arg(config.spec.packets().to_string())
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut rtc = command.spawn().context("failed to start the RTC worker")?;
    let deadline = rtc_worker_deadline(config.spec);
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

enum RtcWorkerCompletion {
    Completed,
    ServerExited(ExitStatus),
}

fn rtc_worker_deadline(spec: ScenarioSpec) -> Duration {
    let peer_count = u64::from(spec.receivers()).saturating_add(1);
    let setup_seconds = peer_count.saturating_mul(PEER_SETUP_TIMEOUT.as_secs());
    let packet_milliseconds = u64::from(spec.packets()).saturating_mul(20);
    Duration::from_secs(setup_seconds)
        .saturating_add(Duration::from_millis(packet_milliseconds))
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

async fn remove_stale_result(result_path: &Path) -> Result<()> {
    match fs::remove_file(result_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove stale result.json"),
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
