use std::{
    fs::{File, create_dir_all},
    io::{ErrorKind, Write as _},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use o_sfu::http::telemetry::{
    diagnostics::{DiagnosticsWorkerSummary, route as diagnostics_route},
    metrics,
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    process::Command,
    sync::oneshot,
    task::JoinHandle,
    time::{Instant as TokioInstant, MissedTickBehavior, interval_at},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(750);

const RTP_PACKETS_INGRESS: &str = "osfu_rtp_packets_total{direction=\"ingress\"}";
const RTP_PACKETS_EGRESS: &str = "osfu_rtp_packets_total{direction=\"egress\"}";
const RTP_BYTES_INGRESS: &str = "osfu_rtp_payload_bytes_total{direction=\"ingress\"}";
const RTP_BYTES_EGRESS: &str = "osfu_rtp_payload_bytes_total{direction=\"egress\"}";
const RTP_FORWARDED_LOCAL: &str = "osfu_rtp_forwarded_packets_total{destination=\"local_rtc\"}";
const RTP_FORWARDED_BYTES_LOCAL: &str =
    "osfu_rtp_forwarded_payload_bytes_total{destination=\"local_rtc\"}";

#[derive(Debug)]
pub struct TelemetryConfig {
    pub base_url: String,
    pub server_pid: u32,
    pub rtc_pid: u32,
    pub output_path: PathBuf,
}

impl TelemetryConfig {
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        server_pid: u32,
        rtc_pid: u32,
        output_path: impl Into<PathBuf>,
    ) -> Self {
        let base_url = base_url.into();
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            server_pid,
            rtc_pid,
            output_path: output_path.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSample {
    pub cpu_ticks: u64,
    pub rss_bytes: u64,
    pub start_time_ticks: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtpCounters {
    pub packets: u64,
    pub payload_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSample {
    pub ingress: RtpCounters,
    pub egress: RtpCounters,
    pub forwarded_local_rtc: RtpCounters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPressureSample {
    pub media_worker_id: usize,
    pub egress_bitrate_bps: u64,
    pub packet_loop_delay_ms: Option<u64>,
    pub command_backlog_depth: usize,
    pub relay_mailbox_depth: usize,
    pub worker_pressure_score: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TelemetryOutcome {
    Sample {
        clock_ticks_per_second: u64,
        server_cpu_percent_milli: Option<u64>,
        rtc_cpu_percent_milli: Option<u64>,
        server_rss_bytes: u64,
        rtc_rss_bytes: Option<u64>,
        server: ProcessSample,
        rtc: Option<ProcessSample>,
        traffic: TrafficSample,
        workers: Vec<WorkerPressureSample>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryRecord {
    pub elapsed_ms: u64,
    pub scrape_duration_ms: u64,
    pub final_sample: bool,
    #[serde(flatten)]
    pub outcome: TelemetryOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TelemetrySummary {
    pub sample_count: usize,
    pub error_count: usize,
    pub errors: Vec<String>,
}

pub struct TelemetrySampler {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<TelemetrySummary>>,
}

impl TelemetrySampler {
    /// Starts one immediate observation followed by one observation per second.
    ///
    /// # Errors
    ///
    /// Returns an error when Linux process parameters, the HTTP client or the
    /// output file cannot be initialized.
    pub async fn start(config: TelemetryConfig) -> Result<Self> {
        let params = SystemParameters::load().await?;
        if let Some(parent) = config
            .output_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let output = File::create(&config.output_path)
            .with_context(|| format!("failed to create {}", config.output_path.display()))?;
        let client = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build the telemetry HTTP client")?;
        let started_at = Instant::now();
        let mut state = SamplerState {
            client,
            metrics_url: format!("{}{}", config.base_url, metrics::PATH),
            workers_url: format!("{}{}", config.base_url, diagnostics_route::WORKERS),
            server_pid: config.server_pid,
            rtc_pid: config.rtc_pid,
            params,
            output,
            started_at,
            server_start_time: None,
            rtc_start_time: None,
            previous_server: None,
            previous_rtc: None,
            sample_count: 0,
            error_count: 0,
            errors: Vec::new(),
        };
        state.record(false).await?;
        let (stop, stop_rx) = oneshot::channel();
        let task = tokio::spawn(run_sampler(state, stop_rx));
        Ok(Self { stop, task })
    }

    /// Stops periodic sampling after one final observation.
    ///
    /// Collection failures are returned in [`TelemetrySummary::errors`] after
    /// being written as error records. File and task failures return an error.
    ///
    /// # Errors
    ///
    /// Returns an error when the sampler task or output writer fails.
    pub async fn finish(self) -> Result<TelemetrySummary> {
        let Self { stop, task } = self;
        let signaled = stop.send(()).is_ok();
        let summary = task.await.context("telemetry sampler task failed")??;
        ensure!(signaled, "telemetry sampler stopped before finish");
        Ok(summary)
    }
}

#[derive(Clone, Copy)]
struct SystemParameters {
    page_size: u64,
    clock_ticks_per_second: u64,
}

impl SystemParameters {
    async fn load() -> Result<Self> {
        let output = Command::new("getconf")
            .arg("-a")
            .output()
            .await
            .context("failed to execute getconf -a")?;
        ensure!(
            output.status.success(),
            "getconf -a exited with {}",
            output.status
        );
        let output =
            String::from_utf8(output.stdout).context("getconf returned non-UTF-8 output")?;
        let params = parse_system_parameters(&output)?;
        ensure!(params.page_size > 0, "getconf returned a zero page size");
        ensure!(
            params.clock_ticks_per_second > 0,
            "getconf returned a zero clock tick rate"
        );
        Ok(params)
    }
}

struct SamplerState {
    client: reqwest::Client,
    metrics_url: String,
    workers_url: String,
    server_pid: u32,
    rtc_pid: u32,
    params: SystemParameters,
    output: File,
    started_at: Instant,
    server_start_time: Option<u64>,
    rtc_start_time: Option<u64>,
    previous_server: Option<TimedProcessSample>,
    previous_rtc: Option<TimedProcessSample>,
    sample_count: usize,
    error_count: usize,
    errors: Vec<String>,
}

impl SamplerState {
    async fn record(&mut self, final_sample: bool) -> Result<()> {
        let elapsed_ms = millis(self.started_at.elapsed());
        let scrape_started = Instant::now();
        let outcome = match self.capture(final_sample, elapsed_ms).await {
            Ok(outcome) => {
                self.sample_count = self.sample_count.saturating_add(1);
                outcome
            }
            Err(error) => {
                let message = format!("{error:#}");
                self.error_count = self.error_count.saturating_add(1);
                self.errors.push(message.clone());
                TelemetryOutcome::Error { message }
            }
        };
        let record = TelemetryRecord {
            elapsed_ms,
            scrape_duration_ms: millis(scrape_started.elapsed()),
            final_sample,
            outcome,
        };
        serde_json::to_writer(&mut self.output, &record)
            .context("failed to encode a telemetry record")?;
        self.output
            .write_all(b"\n")
            .context("failed to terminate a telemetry record")?;
        self.output
            .flush()
            .context("failed to flush telemetry records")
    }

    async fn capture(&mut self, final_sample: bool, elapsed_ms: u64) -> Result<TelemetryOutcome> {
        let server = read_process(self.server_pid, self.params.page_size);
        let rtc = read_process(self.rtc_pid, self.params.page_size);
        let traffic = fetch_traffic(&self.client, &self.metrics_url);
        let workers = fetch_workers(&self.client, &self.workers_url);
        let (server, rtc, traffic, workers) = tokio::join!(server, rtc, traffic, workers);

        let server =
            server?.with_context(|| format!("server process {} disappeared", self.server_pid))?;
        verify_start_time(&mut self.server_start_time, server, "server")?;
        let rtc = match rtc? {
            Some(sample) => {
                verify_start_time(&mut self.rtc_start_time, sample, "RTC generator")?;
                Some(sample)
            }
            None if final_sample => None,
            None => anyhow::bail!("RTC generator process {} disappeared", self.rtc_pid),
        };
        let traffic = traffic?;
        let workers = workers?;
        let server_cpu_percent_milli = cpu_percent_milli(
            self.previous_server,
            elapsed_ms,
            server,
            self.params.clock_ticks_per_second,
        )?;
        let rtc_cpu_percent_milli = rtc
            .map(|rtc| {
                cpu_percent_milli(
                    self.previous_rtc,
                    elapsed_ms,
                    rtc,
                    self.params.clock_ticks_per_second,
                )
            })
            .transpose()?
            .flatten();
        advance_process_sample(&mut self.previous_server, elapsed_ms, server);
        if let Some(rtc) = rtc {
            advance_process_sample(&mut self.previous_rtc, elapsed_ms, rtc);
        }
        Ok(TelemetryOutcome::Sample {
            clock_ticks_per_second: self.params.clock_ticks_per_second,
            server_cpu_percent_milli,
            rtc_cpu_percent_milli,
            server_rss_bytes: server.rss_bytes,
            rtc_rss_bytes: rtc.map(|sample| sample.rss_bytes),
            server,
            rtc,
            traffic,
            workers,
        })
    }

    fn finish(self) -> TelemetrySummary {
        TelemetrySummary {
            sample_count: self.sample_count,
            error_count: self.error_count,
            errors: self.errors,
        }
    }
}

#[derive(Clone, Copy)]
struct TimedProcessSample {
    elapsed_ms: u64,
    process: ProcessSample,
}

fn cpu_percent_milli(
    previous: Option<TimedProcessSample>,
    elapsed_ms: u64,
    process: ProcessSample,
    clock_ticks_per_second: u64,
) -> Result<Option<u64>> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    ensure!(
        previous.process.start_time_ticks == process.start_time_ticks,
        "process start time changed while calculating CPU usage"
    );
    let elapsed_ms = elapsed_ms
        .checked_sub(previous.elapsed_ms)
        .context("telemetry elapsed time moved backwards")?;
    if elapsed_ms == 0 {
        return Ok(None);
    }
    let cpu_ticks = process
        .cpu_ticks
        .checked_sub(previous.process.cpu_ticks)
        .context("process CPU ticks moved backwards")?;
    let numerator = u128::from(cpu_ticks).saturating_mul(100_000_000);
    let denominator = u128::from(clock_ticks_per_second).saturating_mul(u128::from(elapsed_ms));
    let value = numerator
        .checked_div(denominator)
        .context("process CPU rate has a zero divisor")?;
    Ok(Some(u64::try_from(value).unwrap_or(u64::MAX)))
}

fn advance_process_sample(
    previous: &mut Option<TimedProcessSample>,
    elapsed_ms: u64,
    process: ProcessSample,
) {
    if previous.is_none_or(|previous| elapsed_ms > previous.elapsed_ms) {
        *previous = Some(TimedProcessSample {
            elapsed_ms,
            process,
        });
    }
}

async fn run_sampler(
    mut state: SamplerState,
    mut stop: oneshot::Receiver<()>,
) -> Result<TelemetrySummary> {
    let mut interval = interval_at(TokioInstant::now() + SAMPLE_INTERVAL, SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                state.record(true).await?;
                return Ok(state.finish());
            }
            _ = interval.tick() => state.record(false).await?,
        }
    }
}

fn parse_system_parameters(output: &str) -> Result<SystemParameters> {
    let mut page_size = None;
    let mut clock_ticks_per_second = None;
    for line in output.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let name = name.trim_end_matches(':');
        if name == "CLK_TCK" {
            clock_ticks_per_second = fields.next().and_then(|value| value.parse().ok());
        } else if matches!(name, "PAGESIZE" | "PAGE_SIZE") && page_size.is_none() {
            page_size = fields.next().and_then(|value| value.parse().ok());
        }
    }
    Ok(SystemParameters {
        page_size: page_size.context("getconf omitted PAGESIZE")?,
        clock_ticks_per_second: clock_ticks_per_second.context("getconf omitted CLK_TCK")?,
    })
}

async fn read_process(pid: u32, page_size: u64) -> Result<Option<ProcessSample>> {
    let path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&path).await {
        Ok(stat) => stat,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {path}")),
    };
    parse_process_stat(&stat, page_size)
        .with_context(|| format!("failed to parse {path}"))
        .map(Some)
}

fn parse_process_stat(stat: &str, page_size: u64) -> Result<ProcessSample> {
    let command_end = stat
        .rfind(") ")
        .context("process stat has no command terminator")?;
    let fields = stat
        .get(command_end.saturating_add(2)..)
        .context("process stat command terminator is invalid")?;
    let mut utime = None;
    let mut stime = None;
    let mut start_time = None;
    let mut rss_pages = None;
    for (index, value) in fields.split_ascii_whitespace().enumerate() {
        match index.saturating_add(3) {
            14 => utime = Some(value.parse::<u64>().context("invalid process utime")?),
            15 => stime = Some(value.parse::<u64>().context("invalid process stime")?),
            22 => {
                start_time = Some(value.parse::<u64>().context("invalid process start time")?);
            }
            24 => rss_pages = Some(value.parse::<i64>().context("invalid process RSS")?),
            _ => {}
        }
    }
    let cpu_ticks = utime
        .context("process stat is missing utime")?
        .checked_add(stime.context("process stat is missing stime")?)
        .context("process CPU ticks overflowed")?;
    let rss_pages = u64::try_from(rss_pages.context("process stat is missing RSS")?)
        .context("process stat reported a negative RSS")?;
    Ok(ProcessSample {
        cpu_ticks,
        rss_bytes: rss_pages
            .checked_mul(page_size)
            .context("process RSS byte count overflowed")?,
        start_time_ticks: start_time.context("process stat is missing start time")?,
    })
}

fn verify_start_time(
    expected: &mut Option<u64>,
    sample: ProcessSample,
    process: &str,
) -> Result<()> {
    if let Some(expected) = *expected {
        ensure!(
            sample.start_time_ticks == expected,
            "{process} PID was reused: expected start time {expected}, got {}",
            sample.start_time_ticks
        );
    } else {
        *expected = Some(sample.start_time_ticks);
    }
    Ok(())
}

async fn fetch_traffic(client: &reqwest::Client, url: &str) -> Result<TrafficSample> {
    let response = client
        .get(url)
        .send()
        .await
        .context("failed to fetch o-sfu metrics")?
        .error_for_status()
        .context("o-sfu metrics returned an error status")?;
    let payload = response
        .text()
        .await
        .context("failed to read o-sfu metrics")?;
    parse_traffic(&payload)
}

async fn fetch_workers(client: &reqwest::Client, url: &str) -> Result<Vec<WorkerPressureSample>> {
    let response = client
        .get(url)
        .send()
        .await
        .context("failed to fetch o-sfu worker diagnostics")?
        .error_for_status()
        .context("o-sfu worker diagnostics returned an error status")?;
    let mut workers = response
        .json::<Vec<DiagnosticsWorkerSummary>>()
        .await
        .context("failed to decode o-sfu worker diagnostics")?
        .into_iter()
        .map(|worker| {
            let pressure = worker.pressure;
            WorkerPressureSample {
                media_worker_id: worker.media_worker_id,
                egress_bitrate_bps: pressure.egress_bitrate_bps,
                packet_loop_delay_ms: pressure.packet_loop_delay_ms,
                command_backlog_depth: pressure.command_backlog_depth,
                relay_mailbox_depth: pressure.relay_mailbox_depth,
                worker_pressure_score: pressure.worker_pressure_score,
            }
        })
        .collect::<Vec<_>>();
    workers.sort_unstable_by_key(|worker| worker.media_worker_id);
    Ok(workers)
}

#[derive(Default)]
struct TrafficSlots {
    ingress_packets: Option<u64>,
    ingress_bytes: Option<u64>,
    egress_packets: Option<u64>,
    egress_bytes: Option<u64>,
    forwarded_packets: Option<u64>,
    forwarded_bytes: Option<u64>,
}

fn parse_traffic(payload: &str) -> Result<TrafficSample> {
    let mut slots = TrafficSlots::default();
    for line in payload.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let Some(series) = fields.next() else {
            continue;
        };
        let slot = match series {
            RTP_PACKETS_INGRESS => &mut slots.ingress_packets,
            RTP_BYTES_INGRESS => &mut slots.ingress_bytes,
            RTP_PACKETS_EGRESS => &mut slots.egress_packets,
            RTP_BYTES_EGRESS => &mut slots.egress_bytes,
            RTP_FORWARDED_LOCAL => &mut slots.forwarded_packets,
            RTP_FORWARDED_BYTES_LOCAL => &mut slots.forwarded_bytes,
            _ => continue,
        };
        let value = fields
            .next()
            .with_context(|| format!("metric {series} has no value"))?
            .parse::<u64>()
            .with_context(|| format!("metric {series} has a non-counter value"))?;
        ensure!(
            fields.next().is_none(),
            "metric {series} has an unexpected timestamp"
        );
        ensure!(
            slot.replace(value).is_none(),
            "metric {series} is duplicated"
        );
    }
    Ok(TrafficSample {
        ingress: RtpCounters {
            packets: required_counter(slots.ingress_packets, RTP_PACKETS_INGRESS)?,
            payload_bytes: required_counter(slots.ingress_bytes, RTP_BYTES_INGRESS)?,
        },
        egress: RtpCounters {
            packets: required_counter(slots.egress_packets, RTP_PACKETS_EGRESS)?,
            payload_bytes: required_counter(slots.egress_bytes, RTP_BYTES_EGRESS)?,
        },
        forwarded_local_rtc: RtpCounters {
            packets: required_counter(slots.forwarded_packets, RTP_FORWARDED_LOCAL)?,
            payload_bytes: required_counter(slots.forwarded_bytes, RTP_FORWARDED_BYTES_LOCAL)?,
        },
    })
}

fn required_counter(value: Option<u64>, series: &str) -> Result<u64> {
    value.with_context(|| format!("metrics payload is missing {series}"))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "TESTS/telemetry_tests.rs"]
mod tests;
