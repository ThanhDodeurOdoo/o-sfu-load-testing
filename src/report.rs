use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    result::Result as StdResult,
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::{
    AUDIO_PACKET_PAYLOAD_BYTES, AUDIO_PACKETS_PER_SECOND, ScenarioResult, ScenarioSpec,
    VIDEO_FRAMES_PER_SECOND, VIDEO_HIGH_PACKET_PAYLOAD_BYTES, VIDEO_KEYFRAME_INTERVAL,
    VIDEO_LOW_PACKET_PAYLOAD_BYTES,
    phase::ScenarioPhase,
    telemetry::{
        TELEMETRY_SCHEMA_VERSION, TelemetryOutcome, TelemetryRecord, WorkerPressureSample,
    },
    video_packets_per_layer,
};

pub(crate) mod dashboard;
pub use dashboard::DashboardConfig;

const CATEGORY_CHART_POINTS: usize = 4;
const GITHUB_SUMMARY_LIMIT_BYTES: usize = 1024 * 1024;
const RESULT_LIMIT_BYTES: u64 = 1024 * 1024;
const SAMPLES_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_INPUTS: usize = 256;
pub(crate) const MAX_CHART_SCENARIOS: usize = 12;
const MAX_TELEMETRY_SAMPLES: usize = 10_000;
const MAX_TELEMETRY_ERRORS: usize = 8;
const MAX_TELEMETRY_WORKERS: usize = 64;
const MAX_MERMAID_INTEGER: u64 = 9_007_199_254_740_991;
const LINE_COLORS: [(&str, &str); 2] = [("Blue", "#388BFD"), ("Orange", "#B86E00")];

#[derive(Clone)]
pub(crate) struct RunData {
    pub(crate) source: String,
    pub(crate) result: ScenarioResult,
    pub(crate) samples: Option<SampleSet>,
}

pub(crate) struct LoadFailure {
    pub(crate) source: String,
    pub(crate) error: String,
}

#[derive(Clone)]
pub(crate) struct SampleSet {
    samples: Vec<TelemetrySample>,
    phases: Vec<PhaseMarker>,
    unavailable: usize,
    errors: Vec<String>,
}

#[derive(Clone, Default)]
struct TelemetrySample {
    elapsed_ms: u64,
    is_gap: bool,
    final_sample: Option<bool>,
    scrape_duration_ms: Option<u64>,
    clock_ticks_per_second: Option<u64>,
    server_cpu_ticks: Option<u64>,
    server_start_time_ticks: Option<u64>,
    server_rss_bytes: Option<u64>,
    rtc_cpu_ticks: Option<u64>,
    rtc_rss_bytes: Option<u64>,
    server_cpu_percent_milli: Option<u64>,
    rtc_cpu_percent_milli: Option<u64>,
    ingress_packets: Option<u64>,
    ingress_payload_bytes: Option<u64>,
    egress_packets: Option<u64>,
    forwarded_packets: Option<u64>,
    egress_payload_bytes: Option<u64>,
    forwarded_payload_bytes: Option<u64>,
    workers: Vec<WorkerTelemetry>,
    packet_loop_delay_ms: Option<u64>,
    packet_loop_unresponsive: bool,
}

#[derive(Clone, Copy)]
struct WorkerTelemetry {
    media_worker_id: usize,
    egress_bitrate_bps: u64,
    command_backlog_depth: usize,
    relay_mailbox_depth: usize,
    worker_pressure_score: u8,
}

#[derive(Clone, Copy)]
struct PhaseMarker {
    elapsed_ms: u64,
    phase: ScenarioPhase,
}

pub(crate) struct TelemetrySummary {
    pub(crate) samples: usize,
    pub(crate) unavailable: usize,
    pub(crate) elapsed_ms: Option<u64>,
    pub(crate) server_ticks_observed: bool,
    pub(crate) rtc_ticks_observed: bool,
    pub(crate) server_cpu_percent_milli: Option<u64>,
    pub(crate) server_cpu_peak_percent_milli: Option<u64>,
    pub(crate) server_rss_bytes: Option<u64>,
    pub(crate) rtc_cpu_percent_milli: Option<u64>,
    pub(crate) rtc_rss_bytes: Option<u64>,
    pub(crate) deliveries_per_server_cpu_second: Option<u64>,
    pub(crate) server_cpu_micros_per_million_deliveries: Option<u64>,
    pub(crate) forwarded_packets_per_second: Option<u64>,
    pub(crate) egress_payload_bits_per_second: Option<u64>,
    pub(crate) packet_loop_delay_ms: Option<u64>,
    pub(crate) packet_loop_unresponsive_samples: usize,
}

pub(crate) struct ChartSeries<'a> {
    pub(crate) name: &'a str,
    pub(crate) values: &'a [u64],
}

/// Renders one GitHub job summary from result files or result directories.
///
/// Individual input failures are retained in the rendered summary.
///
/// # Errors
///
/// Returns an error when no input is provided, more than 256 inputs are
/// provided, the artifact URL is unsafe for Markdown or the summary exceeds
/// GitHub's one MiB limit.
pub fn render(inputs: &[PathBuf], artifact_url: Option<&str>) -> Result<String> {
    ensure!(!inputs.is_empty(), "at least one report input is required");
    ensure!(
        inputs.len() <= MAX_INPUTS,
        "at most 256 report inputs are allowed"
    );
    validate_artifact_url(artifact_url)?;
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for input in inputs {
        match load_run(input) {
            Ok(run) => runs.push(run),
            Err(error) => failures.push(LoadFailure {
                source: input.display().to_string(),
                error: format!("{error:#}"),
            }),
        }
    }
    render_report(runs, failures, artifact_url)
}

/// Writes one GitHub job summary to `output`.
///
/// # Errors
///
/// Returns an error when rendering, directory creation or persistence fails.
pub fn write(
    inputs: &[PathBuf],
    output: &Path,
    artifact_url: Option<&str>,
    dashboards: Option<&DashboardConfig<'_>>,
) -> Result<()> {
    let mut summary = render(inputs, artifact_url)?;
    if let Some(config) = dashboards {
        summary.push_str(&dashboard::write_single(inputs, config, artifact_url)?);
        ensure_summary_size(&summary)?;
    }
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("failed to create the report directory")?;
    }
    fs::write(output, summary).context("failed to write the report")
}

pub(crate) fn load_run(input: &Path) -> Result<RunData> {
    let result_path = if input.is_dir() {
        input.join("result.json")
    } else {
        input.to_owned()
    };
    let payload = read_bounded(&result_path, RESULT_LIMIT_BYTES)?;
    let result = serde_json::from_slice::<ScenarioResult>(&payload)
        .with_context(|| format!("failed to decode {}", result_path.display()))?;
    let samples_path = result_path.with_file_name("samples.jsonl");
    let samples = read_optional_bounded(&samples_path, SAMPLES_LIMIT_BYTES)?
        .map(|payload| parse_samples(&payload));
    Ok(RunData {
        source: input.display().to_string(),
        result,
        samples,
    })
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.len() <= limit,
        "{} exceeds its {} byte report-input limit",
        path.display(),
        limit
    );
    let payload = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        u64::try_from(payload.len()).unwrap_or(u64::MAX) <= limit,
        "{} grew beyond its {} byte report-input limit",
        path.display(),
        limit
    );
    Ok(payload)
}

fn read_optional_bounded(path: &Path, limit: u64) -> Result<Option<String>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    ensure!(
        metadata.len() <= limit,
        "{} exceeds its {} byte report-input limit",
        path.display(),
        limit
    );
    let payload = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        u64::try_from(payload.len()).unwrap_or(u64::MAX) <= limit,
        "{} grew beyond its {} byte report-input limit",
        path.display(),
        limit
    );
    String::from_utf8(payload)
        .map(Some)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

pub(crate) fn parse_samples(payload: &str) -> SampleSet {
    let mut samples = Vec::new();
    let mut phases = Vec::new();
    let mut unavailable = 0_usize;
    let mut errors = Vec::new();
    let mut records = 0_usize;
    let mut phases_required = false;
    for line in payload
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if records >= MAX_TELEMETRY_SAMPLES {
            unavailable = unavailable.saturating_add(1);
            retain_error(&mut errors, "telemetry sample limit reached".to_owned());
            continue;
        }
        records = records.saturating_add(1);
        match serde_json::from_str::<Value>(line) {
            Ok(value) if value.get("status").is_none() => {
                if let Some(sample) = TelemetrySample::from_value(&value) {
                    samples.push(sample);
                } else {
                    unavailable = unavailable.saturating_add(1);
                    retain_error(&mut errors, telemetry_error(&value));
                    if let Some(sample) = TelemetrySample::gap_from_value(&value) {
                        samples.push(sample);
                    }
                }
            }
            Ok(value) => {
                phases_required |= parse_current_record(
                    value,
                    &mut samples,
                    &mut phases,
                    &mut unavailable,
                    &mut errors,
                );
            }
            Err(error) => {
                unavailable = unavailable.saturating_add(1);
                retain_error(&mut errors, format!("malformed telemetry record: {error}"));
            }
        }
    }
    if (phases_required || !phases.is_empty()) && phases.len() != ScenarioPhase::COUNT {
        unavailable = unavailable.saturating_add(1);
        retain_error(
            &mut errors,
            "telemetry phase sequence is incomplete".to_owned(),
        );
    }
    samples.sort_by_key(|sample| sample.elapsed_ms);
    SampleSet {
        samples,
        phases,
        unavailable,
        errors,
    }
}

fn parse_current_record(
    value: Value,
    samples: &mut Vec<TelemetrySample>,
    phases: &mut Vec<PhaseMarker>,
    unavailable: &mut usize,
    errors: &mut Vec<String>,
) -> bool {
    let final_sample = value.get("finalSample").and_then(Value::as_bool);
    let gap = TelemetrySample::gap_from_value(&value);
    let record = match serde_json::from_value::<TelemetryRecord>(value) {
        Ok(record) => record,
        Err(error) => {
            *unavailable = unavailable.saturating_add(1);
            retain_error(errors, format!("malformed telemetry record: {error}"));
            if let Some(sample) = gap {
                samples.push(sample);
            }
            return false;
        }
    };
    let TelemetryRecord {
        schema_version,
        elapsed_ms,
        scrape_duration_ms,
        outcome,
        final_sample: _,
    } = record;
    match outcome {
        outcome @ TelemetryOutcome::Sample { .. } => {
            if let Some(sample) =
                TelemetrySample::from_current(elapsed_ms, scrape_duration_ms, final_sample, outcome)
            {
                samples.push(sample);
            } else {
                *unavailable = unavailable.saturating_add(1);
                retain_error(
                    errors,
                    "telemetry sample contains invalid worker data".to_owned(),
                );
                if let Some(sample) = gap {
                    samples.push(sample);
                }
            }
        }
        TelemetryOutcome::Error { message } => {
            *unavailable = unavailable.saturating_add(1);
            retain_error(errors, message);
            if let Some(sample) = gap {
                samples.push(sample);
            }
        }
        TelemetryOutcome::Phase { phase } => {
            match PhaseMarker::new(elapsed_ms, phase, phases.last()) {
                Ok(marker) => phases.push(marker),
                Err(error) => {
                    *unavailable = unavailable.saturating_add(1);
                    retain_error(errors, error);
                }
            }
        }
    }
    schema_version >= TELEMETRY_SCHEMA_VERSION
}

fn retain_error(errors: &mut Vec<String>, error: String) {
    if errors.len() < MAX_TELEMETRY_ERRORS {
        errors.push(error);
    }
}

fn telemetry_error(value: &Value) -> String {
    value.get("message").and_then(Value::as_str).map_or_else(
        || "telemetry record contains no usable process data".to_owned(),
        ToOwned::to_owned,
    )
}

impl TelemetrySample {
    fn gap_from_value(value: &Value) -> Option<Self> {
        Some(Self {
            elapsed_ms: value.get("elapsedMs")?.as_u64()?,
            is_gap: true,
            final_sample: value.get("finalSample").and_then(Value::as_bool),
            scrape_duration_ms: value.get("scrapeDurationMs").and_then(Value::as_u64),
            ..Self::default()
        })
    }

    fn from_current(
        elapsed_ms: u64,
        scrape_duration_ms: u64,
        final_sample: Option<bool>,
        outcome: TelemetryOutcome,
    ) -> Option<Self> {
        let TelemetryOutcome::Sample {
            clock_ticks_per_second,
            server_cpu_percent_milli,
            rtc_cpu_percent_milli,
            server_rss_bytes,
            rtc_rss_bytes,
            server,
            rtc,
            traffic,
            workers,
        } = outcome
        else {
            return None;
        };
        let packet_loop_unresponsive = workers
            .iter()
            .any(|worker| worker.packet_loop_delay_ms.is_none());
        let packet_loop_delay_ms = (!packet_loop_unresponsive)
            .then(|| {
                workers
                    .iter()
                    .filter_map(|worker| worker.packet_loop_delay_ms)
                    .max()
            })
            .flatten();
        let workers = WorkerTelemetry::from_current(&workers)?;
        Some(Self {
            elapsed_ms,
            is_gap: false,
            final_sample,
            scrape_duration_ms: Some(scrape_duration_ms),
            clock_ticks_per_second: Some(clock_ticks_per_second),
            server_cpu_ticks: Some(server.cpu_ticks),
            server_start_time_ticks: Some(server.start_time_ticks),
            server_rss_bytes: Some(server_rss_bytes),
            rtc_cpu_ticks: rtc.map(|sample| sample.cpu_ticks),
            rtc_rss_bytes,
            server_cpu_percent_milli,
            rtc_cpu_percent_milli,
            ingress_packets: Some(traffic.ingress.packets),
            ingress_payload_bytes: Some(traffic.ingress.payload_bytes),
            egress_packets: Some(traffic.egress.packets),
            forwarded_packets: Some(traffic.forwarded_local_rtc.packets),
            egress_payload_bytes: Some(traffic.egress.payload_bytes),
            forwarded_payload_bytes: Some(traffic.forwarded_local_rtc.payload_bytes),
            workers,
            packet_loop_delay_ms,
            packet_loop_unresponsive,
        })
    }

    fn from_value(value: &Value) -> Option<Self> {
        let workers = match value.get("workers") {
            Some(workers) => WorkerTelemetry::from_values(workers.as_array()?)?,
            None => Vec::new(),
        };
        let packet_loop_unresponsive =
            value
                .get("workers")
                .and_then(Value::as_array)
                .is_some_and(|workers| {
                    workers
                        .iter()
                        .any(|worker| worker.get("packetLoopDelayMs").is_some_and(Value::is_null))
                });
        let sample = Self {
            elapsed_ms: value.get("elapsedMs")?.as_u64()?,
            is_gap: false,
            final_sample: value.get("finalSample").and_then(Value::as_bool),
            scrape_duration_ms: value.get("scrapeDurationMs").and_then(Value::as_u64),
            clock_ticks_per_second: value.get("clockTicksPerSecond").and_then(Value::as_u64),
            server_cpu_ticks: nested_u64(value, "server", "cpuTicks"),
            server_start_time_ticks: nested_u64(value, "server", "startTimeTicks"),
            server_rss_bytes: nested_u64(value, "server", "rssBytes")
                .or_else(|| value.get("serverRssBytes").and_then(Value::as_u64)),
            rtc_cpu_ticks: nested_u64(value, "rtc", "cpuTicks"),
            rtc_rss_bytes: nested_u64(value, "rtc", "rssBytes")
                .or_else(|| value.get("rtcRssBytes").and_then(Value::as_u64)),
            server_cpu_percent_milli: value.get("serverCpuPercentMilli").and_then(Value::as_u64),
            rtc_cpu_percent_milli: value.get("rtcCpuPercentMilli").and_then(Value::as_u64),
            ingress_packets: traffic_u64(value, "ingress", "packets"),
            ingress_payload_bytes: traffic_u64(value, "ingress", "payloadBytes"),
            egress_packets: traffic_u64(value, "egress", "packets"),
            forwarded_packets: traffic_u64(value, "forwardedLocalRtc", "packets"),
            egress_payload_bytes: traffic_u64(value, "egress", "payloadBytes"),
            forwarded_payload_bytes: traffic_u64(value, "forwardedLocalRtc", "payloadBytes"),
            workers,
            packet_loop_delay_ms: (!packet_loop_unresponsive)
                .then(|| {
                    value
                        .get("workers")
                        .and_then(Value::as_array)
                        .and_then(|workers| {
                            workers
                                .iter()
                                .filter_map(|worker| worker.get("packetLoopDelayMs"))
                                .filter_map(Value::as_u64)
                                .max()
                        })
                })
                .flatten(),
            packet_loop_unresponsive,
        };
        (sample.server_cpu_ticks.is_some()
            || sample.server_rss_bytes.is_some()
            || sample.rtc_cpu_ticks.is_some()
            || sample.rtc_rss_bytes.is_some()
            || sample.server_cpu_percent_milli.is_some()
            || sample.rtc_cpu_percent_milli.is_some()
            || sample.ingress_packets.is_some()
            || sample.ingress_payload_bytes.is_some()
            || sample.egress_packets.is_some()
            || sample.forwarded_packets.is_some()
            || sample.egress_payload_bytes.is_some()
            || sample.forwarded_payload_bytes.is_some()
            || !sample.workers.is_empty()
            || sample.packet_loop_delay_ms.is_some()
            || sample.packet_loop_unresponsive
            || sample.scrape_duration_ms.is_some())
        .then_some(sample)
    }
}

impl WorkerTelemetry {
    fn from_values(values: &[Value]) -> Option<Vec<Self>> {
        if values.len() > MAX_TELEMETRY_WORKERS {
            return None;
        }
        let workers = values
            .iter()
            .map(Self::from_value)
            .collect::<Option<Vec<_>>>()?;
        Self::valid(&workers).then_some(workers)
    }

    fn from_current(values: &[WorkerPressureSample]) -> Option<Vec<Self>> {
        if values.len() > MAX_TELEMETRY_WORKERS {
            return None;
        }
        let workers = values
            .iter()
            .map(|worker| Self {
                media_worker_id: worker.media_worker_id,
                egress_bitrate_bps: worker.egress_bitrate_bps,
                command_backlog_depth: worker.command_backlog_depth,
                relay_mailbox_depth: worker.relay_mailbox_depth,
                worker_pressure_score: worker.worker_pressure_score,
            })
            .collect::<Vec<_>>();
        Self::valid(&workers).then_some(workers)
    }

    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            media_worker_id: usize::try_from(value.get("mediaWorkerId")?.as_u64()?).ok()?,
            egress_bitrate_bps: value.get("egressBitrateBps")?.as_u64()?,
            command_backlog_depth: usize::try_from(value.get("commandBacklogDepth")?.as_u64()?)
                .ok()?,
            relay_mailbox_depth: usize::try_from(value.get("relayMailboxDepth")?.as_u64()?).ok()?,
            worker_pressure_score: u8::try_from(value.get("workerPressureScore")?.as_u64()?)
                .ok()?,
        })
    }

    fn valid(workers: &[Self]) -> bool {
        workers
            .iter()
            .all(|worker| worker.media_worker_id < MAX_TELEMETRY_WORKERS)
            && workers
                .iter()
                .map(|worker| worker.media_worker_id)
                .collect::<BTreeSet<_>>()
                .len()
                == workers.len()
    }
}

impl PhaseMarker {
    fn new(
        elapsed_ms: u64,
        phase: ScenarioPhase,
        previous: Option<&Self>,
    ) -> StdResult<Self, String> {
        let ordinal = previous.map_or(0, |marker| marker.phase.ordinal().saturating_add(1));
        let expected = ScenarioPhase::ORDERED
            .get(ordinal)
            .ok_or_else(|| "telemetry contains more than four phase markers".to_owned())?;
        if phase != *expected {
            return Err(format!(
                "telemetry phase is out of order: expected {expected}, got {phase}"
            ));
        }
        if previous.is_some_and(|previous| elapsed_ms < previous.elapsed_ms) {
            return Err("telemetry phase elapsed time moved backwards".to_owned());
        }
        Ok(Self { elapsed_ms, phase })
    }
}

impl TelemetrySummary {
    pub(crate) fn from_samples(sample_set: Option<&SampleSet>, delivered_packets: u64) -> Self {
        let Some(sample_set) = sample_set else {
            return Self {
                samples: 0,
                unavailable: 0,
                elapsed_ms: None,
                server_ticks_observed: false,
                rtc_ticks_observed: false,
                server_cpu_percent_milli: None,
                server_cpu_peak_percent_milli: None,
                server_rss_bytes: None,
                rtc_cpu_percent_milli: None,
                rtc_rss_bytes: None,
                deliveries_per_server_cpu_second: None,
                server_cpu_micros_per_million_deliveries: None,
                forwarded_packets_per_second: None,
                egress_payload_bits_per_second: None,
                packet_loop_delay_ms: None,
                packet_loop_unresponsive_samples: 0,
            };
        };
        let samples = &sample_set.samples;
        let server_cpu_ticks = server_cpu_ticks(samples);
        Self {
            samples: samples.iter().filter(|sample| !sample.is_gap).count(),
            unavailable: sample_set.unavailable,
            elapsed_ms: samples.last().map(|sample| sample.elapsed_ms),
            server_ticks_observed: samples
                .iter()
                .any(|sample| sample.server_cpu_ticks.is_some()),
            rtc_ticks_observed: samples.iter().any(|sample| sample.rtc_cpu_ticks.is_some()),
            server_cpu_percent_milli: whole_window_average(
                samples,
                |sample| sample.server_cpu_ticks.is_some(),
                |sample| {
                    sample.server_cpu_ticks.is_some() && sample.server_cpu_percent_milli.is_some()
                },
                |sample| sample.server_cpu_percent_milli,
            ),
            server_cpu_peak_percent_milli: samples
                .iter()
                .filter_map(|sample| sample.server_cpu_percent_milli)
                .max(),
            server_rss_bytes: samples
                .iter()
                .filter_map(|sample| sample.server_rss_bytes)
                .max(),
            rtc_cpu_percent_milli: whole_window_average(
                samples,
                |sample| sample.rtc_cpu_ticks.is_some(),
                |sample| sample.rtc_cpu_ticks.is_some() && sample.rtc_cpu_percent_milli.is_some(),
                |sample| sample.rtc_cpu_percent_milli,
            ),
            rtc_rss_bytes: samples
                .iter()
                .filter_map(|sample| sample.rtc_rss_bytes)
                .max(),
            deliveries_per_server_cpu_second: server_cpu_ticks.map(|(ticks, ticks_per_second)| {
                let deliveries = u128::from(delivered_packets) * u128::from(ticks_per_second)
                    / u128::from(ticks);
                u64::try_from(deliveries).unwrap_or(u64::MAX)
            }),
            server_cpu_micros_per_million_deliveries: server_cpu_ticks.and_then(
                |(ticks, ticks_per_second)| {
                    cpu_micros_per_million(ticks, ticks_per_second, delivered_packets)
                },
            ),
            forwarded_packets_per_second: counter_rate(
                samples,
                |sample| sample.forwarded_packets,
                1_000,
            ),
            egress_payload_bits_per_second: counter_rate(
                samples,
                |sample| sample.egress_payload_bytes,
                8_000,
            ),
            packet_loop_delay_ms: samples
                .iter()
                .filter_map(|sample| sample.packet_loop_delay_ms)
                .max(),
            packet_loop_unresponsive_samples: samples
                .iter()
                .filter(|sample| sample.packet_loop_unresponsive)
                .count(),
        }
    }
}

fn nested_u64(value: &Value, object: &str, field: &str) -> Option<u64> {
    value.get(object)?.get(field)?.as_u64()
}

fn traffic_u64(value: &Value, direction: &str, field: &str) -> Option<u64> {
    value.get("traffic")?.get(direction)?.get(field)?.as_u64()
}

fn whole_window_endpoints(
    samples: &[TelemetrySample],
) -> Option<(&TelemetrySample, &TelemetrySample)> {
    let initial = samples.first()?;
    let final_sample = samples.last()?;
    (!initial.is_gap
        && initial.final_sample == Some(false)
        && !final_sample.is_gap
        && final_sample.final_sample == Some(true)
        && final_sample.elapsed_ms > initial.elapsed_ms)
        .then_some((initial, final_sample))
}

fn whole_window_average(
    samples: &[TelemetrySample],
    initial_usable: fn(&TelemetrySample) -> bool,
    final_usable: fn(&TelemetrySample) -> bool,
    value: fn(&TelemetrySample) -> Option<u64>,
) -> Option<u64> {
    let (initial, final_sample) = whole_window_endpoints(samples)?;
    if !initial_usable(initial) || !final_usable(final_sample) {
        return None;
    }
    let elapsed_ms = final_sample.elapsed_ms.checked_sub(initial.elapsed_ms)?;
    let mut previous_elapsed_ms = initial.elapsed_ms;
    let mut weighted_sum = 0_u128;
    let mut total_weight = 0_u128;
    for sample in samples.iter().skip(1) {
        if sample.is_gap {
            continue;
        }
        let weight = sample.elapsed_ms.checked_sub(previous_elapsed_ms)?;
        if weight > 0
            && let Some(value) = value(sample)
        {
            weighted_sum = weighted_sum.saturating_add(u128::from(value) * u128::from(weight));
            total_weight = total_weight.saturating_add(u128::from(weight));
        }
        previous_elapsed_ms = sample.elapsed_ms;
    }
    (total_weight == u128::from(elapsed_ms))
        .then(|| weighted_sum / total_weight)
        .and_then(|average| u64::try_from(average).ok())
}

fn server_cpu_ticks(samples: &[TelemetrySample]) -> Option<(u64, u64)> {
    let usable = |sample: &TelemetrySample| {
        sample.server_cpu_ticks.is_some()
            && sample.server_start_time_ticks.is_some()
            && sample.clock_ticks_per_second.is_some()
    };
    let (initial, final_sample) = whole_window_endpoints(samples)?;
    if !usable(initial) || !usable(final_sample) {
        return None;
    }
    let initial_ticks = initial.server_cpu_ticks?;
    let start_time = initial.server_start_time_ticks?;
    let ticks_per_second = initial.clock_ticks_per_second?;
    if final_sample.server_start_time_ticks? != start_time
        || final_sample.clock_ticks_per_second? != ticks_per_second
        || ticks_per_second == 0
    {
        return None;
    }
    let delta = final_sample.server_cpu_ticks?.checked_sub(initial_ticks)?;
    (delta > 0).then_some((delta, ticks_per_second))
}

fn cpu_micros_per_million(
    ticks: u64,
    ticks_per_second: u64,
    delivered_packets: u64,
) -> Option<u64> {
    if ticks_per_second == 0 || delivered_packets == 0 {
        return None;
    }
    let numerator = u128::from(ticks) * 1_000_000_000_000;
    let denominator = u128::from(ticks_per_second) * u128::from(delivered_packets);
    u64::try_from(numerator / denominator).ok()
}

fn counter_rate(
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
    scale: u64,
) -> Option<u64> {
    let (initial, final_sample) = whole_window_endpoints(samples)?;
    let elapsed_ms = final_sample.elapsed_ms.checked_sub(initial.elapsed_ms)?;
    let delta = value(final_sample)?.checked_sub(value(initial)?)?;
    let rate = u128::from(delta) * u128::from(scale) / u128::from(elapsed_ms);
    Some(u64::try_from(rate).unwrap_or(u64::MAX))
}

fn render_report(
    mut runs: Vec<RunData>,
    mut failures: Vec<LoadFailure>,
    artifact_url: Option<&str>,
) -> Result<String> {
    runs.sort_unstable_by(|left, right| {
        scenario_key(left.result.scenario)
            .cmp(&scenario_key(right.result.scenario))
            .then_with(|| left.result.profile.cmp(&right.result.profile))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| {
                left.result
                    .delivered_packets
                    .cmp(&right.result.delivered_packets)
            })
            .then_with(|| left.result.elapsed_ms.cmp(&right.result.elapsed_ms))
            .then_with(|| format!("{:?}", left.result).cmp(&format!("{:?}", right.result)))
    });
    failures.sort_unstable_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.error.cmp(&right.error))
    });
    let mut output = String::new();
    writeln!(&mut output, "# o-sfu load report\n")?;
    render_status(&mut output, &runs, &failures, artifact_url)?;
    if !failures.is_empty() {
        render_failures(&mut output, &failures)?;
    }
    if runs.is_empty() {
        writeln!(output, "No valid result files were available.\n")?;
    } else {
        render_workloads(&mut output, &runs)?;
    }
    if !runs.is_empty() {
        render_media_profile(&mut output)?;
        render_scenario_legend(&mut output)?;
        render_delivery(&mut output, &runs)?;
        render_discrepancies(&mut output, &runs)?;
        if runs.iter().any(|run| run.samples.is_some()) {
            render_telemetry(&mut output, &runs)?;
        }
    }
    ensure_summary_size(&output)?;
    Ok(output)
}

fn render_status(
    output: &mut String,
    runs: &[RunData],
    failures: &[LoadFailure],
    artifact_url: Option<&str>,
) -> Result<()> {
    let revision = revision_label(runs);
    let telemetry = telemetry_status(runs);
    writeln!(
        output,
        "| Exact work | Scenarios | Failed inputs | Telemetry | o-sfu revision |"
    )?;
    writeln!(output, "| --- | ---: | ---: | --- | --- |")?;
    writeln!(
        output,
        "| {} | {} | {} | {telemetry} | {} |\n",
        exact_status(runs, failures),
        runs.len(),
        failures.len(),
        escape_table(&revision)
    )?;
    writeln!(
        output,
        "Completed performance samples: **{}**. A sample is invalid when send lag exceeds one media interval.\n",
        pacing_status(runs)
    )?;
    if let Some(url) = artifact_url {
        writeln!(output, "[Download raw results and logs]({url})\n")?;
    }
    Ok(())
}

fn exact_status(runs: &[RunData], failures: &[LoadFailure]) -> &'static str {
    if runs.is_empty() && failures.is_empty() {
        "n/a"
    } else if failures.is_empty() && runs.iter().all(run_passed) {
        "PASS"
    } else {
        "FAIL"
    }
}

fn render_failures(output: &mut String, failures: &[LoadFailure]) -> Result<()> {
    writeln!(output, "## Input failures\n")?;
    writeln!(output, "| Input | Error |")?;
    writeln!(output, "| --- | --- |")?;
    for failure in failures {
        writeln!(
            output,
            "| {} | {} |",
            escape_table(&failure.source),
            escape_table(&failure.error)
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn format_packet_loop_health(summary: &TelemetrySummary) -> String {
    if summary.packet_loop_unresponsive_samples == 0 {
        return format_optional(summary.packet_loop_delay_ms, format_milliseconds);
    }
    let numeric = format_optional(summary.packet_loop_delay_ms, format_milliseconds);
    let unit = if summary.packet_loop_unresponsive_samples == 1 {
        "sample"
    } else {
        "samples"
    };
    format!(
        "UNRESPONSIVE ({} {unit}, numeric max {numeric})",
        summary.packet_loop_unresponsive_samples,
    )
}

fn render_workloads(output: &mut String, runs: &[RunData]) -> Result<()> {
    writeln!(output, "## Workloads\n")?;
    render_consumer_context(output)?;
    writeln!(
        output,
        "| Exact | Scenario | Rooms | Peers/room | Publications/room | Streams | Total media consumers | Offered packets | Expected deliveries | Duration | Max send lag | Pacing | Profile | Validation |"
    )?;
    writeln!(
        output,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |"
    )?;
    for run in runs {
        let result = &run.result;
        let scenario = result.scenario;
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} s | {} ms | {} | {} | {} |",
            if run_passed(run) { "PASS" } else { "FAIL" },
            scenario_label(scenario),
            scenario.room_count(),
            scenario.peers_per_room(),
            publisher_label(scenario),
            grouped(result.plan.streams),
            grouped(result.plan.routes),
            grouped(result.plan.offered_packets),
            grouped(result.plan.expected_deliveries),
            scenario.duration_seconds(),
            grouped(result.max_send_lag_ms),
            pacing_label(result),
            escape_table(&result.profile),
            validation_label(run)
        )?;
    }
    writeln!(output)?;
    Ok(())
}

pub(crate) fn render_consumer_context(output: &mut String) -> Result<()> {
    writeln!(
        output,
        "Reports count each simultaneous source-to-receiver forwarding relationship as one media consumer. Each audio or camera publication creates one consumer for every other peer in its room. Publishers do not consume their own publication. A two-RID simulcast camera remains one consumer per receiver because each receiver selects one RID.\n"
    )?;
    writeln!(
        output,
        "For example, 10 audio publishers among 60 total peers create `10 \u{d7} (60 - 1) = 590` consumers. The count would be 600 only for 60 listeners separate from the 10 publishers. [mediasoup documents](https://mediasoup.org/documentation/v3/scalability/) that one single-core worker typically handles over roughly 500 consumers depending on host CPU capability. That figure is topology context, not a portable capacity threshold.\n"
    )?;
    Ok(())
}

pub(crate) fn render_media_profile(output: &mut String) -> Result<()> {
    ensure!(
        1_000_u32.is_multiple_of(AUDIO_PACKETS_PER_SECOND),
        "audio packet rate must contain whole milliseconds"
    );
    let audio_packetization_ms = 1_000 / AUDIO_PACKETS_PER_SECOND;
    ensure!(
        VIDEO_KEYFRAME_INTERVAL.is_multiple_of(u64::from(VIDEO_FRAMES_PER_SECOND)),
        "video keyframe interval must contain complete seconds"
    );
    let profile_seconds = u32::try_from(
        VIDEO_KEYFRAME_INTERVAL
            .checked_div(u64::from(VIDEO_FRAMES_PER_SECOND))
            .context("video keyframe interval is shorter than one second")?,
    )
    .context("video profile duration exceeds u32")?;
    ensure!(
        profile_seconds > 0,
        "video profile duration must be positive"
    );
    let (low_packets, high_packets) = video_packets_per_layer(profile_seconds)?;
    let audio_bps = u64::try_from(AUDIO_PACKET_PAYLOAD_BYTES)
        .context("audio payload size exceeds u64")?
        .checked_mul(u64::from(AUDIO_PACKETS_PER_SECOND))
        .and_then(|bytes| bytes.checked_mul(8))
        .context("audio profile bitrate overflowed")?;
    let low_bps =
        payload_rate_for_profile(low_packets, VIDEO_LOW_PACKET_PAYLOAD_BYTES, profile_seconds)?;
    let high_bps = payload_rate_for_profile(
        high_packets,
        VIDEO_HIGH_PACKET_PAYLOAD_BYTES,
        profile_seconds,
    )?;

    writeln!(output, "## Per-stream media load\n")?;
    writeln!(
        output,
        "Rates are deterministic RTP payload only. They exclude RTP headers, SRTP, UDP, IP, RTCP and retransmissions. One camera publication contains two RTP streams, one per RID.\n"
    )?;
    writeln!(
        output,
        "Fixed payload sizes approximate average active-media output. Browser VBR, DTX and congestion control vary. The profile is a production high-load envelope, not a prediction of average meeting bandwidth.\n"
    )?;
    writeln!(
        output,
        "| Media unit | Payload model | RTP packets/s | RTP payload bitrate |"
    )?;
    writeln!(output, "| --- | --- | ---: | ---: |")?;
    writeln!(
        output,
        "| One Opus audio RTP stream | {} B every {} ms | {} | {} bit/s |",
        AUDIO_PACKET_PAYLOAD_BYTES,
        audio_packetization_ms,
        AUDIO_PACKETS_PER_SECOND,
        grouped(audio_bps)
    )?;
    writeln!(
        output,
        "| One VP8 low RID RTP stream | {} B packets, {} fps | {} | {} bit/s |",
        VIDEO_LOW_PACKET_PAYLOAD_BYTES,
        VIDEO_FRAMES_PER_SECOND,
        format_profile_packet_rate(low_packets, profile_seconds)?,
        grouped(low_bps)
    )?;
    writeln!(
        output,
        "| One VP8 high RID RTP stream | {} B packets, {} fps | {} | {} bit/s |",
        VIDEO_HIGH_PACKET_PAYLOAD_BYTES,
        VIDEO_FRAMES_PER_SECOND,
        format_profile_packet_rate(high_packets, profile_seconds)?,
        grouped(high_bps)
    )?;
    writeln!(
        output,
        "| One VP8 camera publication, two RTP streams | {}-second GOP | {} | {} bit/s |",
        profile_seconds,
        format_profile_packet_rate(
            low_packets
                .checked_add(high_packets)
                .context("camera profile packet count overflowed")?,
            profile_seconds
        )?,
        grouped(
            low_bps
                .checked_add(high_bps)
                .context("camera profile bitrate overflowed")?
        )
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "The audio model is continuous active full-band speech at {} bit/s with the {audio_packetization_ms} ms Opus packetization default. The VP8 profile uses {VIDEO_FRAMES_PER_SECOND} frames/s and one keyframe every {profile_seconds} seconds. Each first keyframe fragment carries the complete ten-byte VP8 uncompressed prefix with the keyframe sync code and 320 by 180 low or 1,280 by 720 high dimensions. Both VP8 RID rates form a high-bitrate simulcast stress envelope. The largest RTP payload is {VIDEO_HIGH_PACKET_PAYLOAD_BYTES} B, which leaves 180 B beneath the 1,280 B IPv6 effective MTU for transport headers and negotiated RTP extensions. [Opus RTP guidance](https://www.rfc-editor.org/rfc/rfc7587.html), [VP8 keyframe prefix](https://www.rfc-editor.org/rfc/rfc6386.html#section-9.1), [VP8 RTP fragmentation](https://www.rfc-editor.org/rfc/rfc7741.html#section-4.4), [UDP message sizing](https://www.rfc-editor.org/rfc/rfc8085.html#section-3.2) and [WebRTC bitrate units](https://www.w3.org/TR/webrtc/#dom-rtcrtpencodingparameters-maxbitrate).\n",
        grouped(audio_bps)
    )?;
    Ok(())
}

pub(crate) fn render_scenario_legend(output: &mut String) -> Result<()> {
    writeln!(output, "## Scenario label legend\n")?;
    writeln!(
        output,
        "Tables use hyphenated IDs. Graphs use `S` for smoke, `A` for audio mesh, `V` for video gallery and `M` for mixed conference.\n"
    )?;
    writeln!(
        output,
        "- `smoke-2r-50p` or `S 2r/50p` means 2 receivers get 50 packets from one publisher."
    )?;
    writeln!(
        output,
        "- `audio-mesh-2x12-60s` or `A 2x12/60s` means 2 rooms with 12 peers per room. Every peer publishes audio for 60 seconds."
    )?;
    writeln!(
        output,
        "- `video-gallery-1x64-10p-60s` or `V 1x64/10p/60s` means 1 room with 64 peers. 10 peers per room publish video for 60 seconds."
    )?;
    writeln!(
        output,
        "- `mixed-conference-1x20-5a-4v-10s` or `M 1x20/5a/4v/10s` means 1 room with 20 peers. 5 publish audio and the first 4 of those also publish video for 10 seconds.\n"
    )?;
    Ok(())
}

fn render_delivery(output: &mut String, runs: &[RunData]) -> Result<()> {
    writeln!(output, "## Exact delivery\n")?;
    let labels = runs
        .iter()
        .map(|run| chart_label(run.result.scenario))
        .collect::<Vec<_>>();
    let delivery_rates = runs
        .iter()
        .map(|run| delivery_rate(&run.result))
        .collect::<Vec<_>>();
    let scheduled_rates = runs
        .iter()
        .map(|run| scheduled_payload_bits_per_second(&run.result) / 1_000)
        .collect::<Vec<_>>();
    let observed_rates = runs
        .iter()
        .map(|run| delivered_payload_bits_per_second(&run.result) / 1_000)
        .collect::<Vec<_>>();
    if runs.len() <= MAX_CHART_SCENARIOS {
        render_category_charts(
            output,
            "Observed receiver deliveries per second",
            "The line compares receiver-observed delivery rates across scenarios.",
            &labels,
            "deliveries/s",
            0,
            &[ChartSeries {
                name: "observed deliveries",
                values: &delivery_rates,
            }],
        )?;
        render_category_charts(
            output,
            "Scheduled sender and receiver-observed RTP payload",
            "The first line is scheduled sender RTP payload. The second line is receiver-observed RTP payload.",
            &labels,
            "RTP payload kbit/s",
            0,
            &[
                ChartSeries {
                    name: "scheduled sender",
                    values: &scheduled_rates,
                },
                ChartSeries {
                    name: "receiver observed",
                    values: &observed_rates,
                },
            ],
        )?;
    } else {
        writeln!(
            output,
            "Visual charts are omitted when more than {MAX_CHART_SCENARIOS} scenarios are reported.\n"
        )?;
    }
    writeln!(
        output,
        "| Scenario | Expected | Delivered | Delivery rate | Scheduled sender payload | Receiver-observed payload | Exact |"
    )?;
    writeln!(output, "| --- | ---: | ---: | ---: | ---: | ---: | --- |")?;
    for run in runs {
        let result = &run.result;
        writeln!(
            output,
            "| {} | {} | {} | {}/s | {} | {} | {} |",
            scenario_label(result.scenario),
            grouped(result.plan.expected_deliveries),
            grouped(result.delivered_packets),
            grouped(delivery_rate(result)),
            format_bits_per_second(scheduled_payload_bits_per_second(result)),
            format_bits_per_second(delivered_payload_bits_per_second(result)),
            if run_passed(run) { "PASS" } else { "FAIL" }
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_discrepancies(output: &mut String, runs: &[RunData]) -> Result<()> {
    writeln!(output, "## Packet discrepancies\n")?;
    writeln!(
        output,
        "| Scenario | Missing | Duplicate | Out of order | Unexpected | Payload mismatch | Total |"
    )?;
    writeln!(output, "| --- | ---: | ---: | ---: | ---: | ---: | ---: |")?;
    for run in runs {
        let correctness = run.result.correctness;
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} |",
            scenario_label(run.result.scenario),
            grouped(correctness.missing_packets),
            grouped(correctness.duplicate_packets),
            grouped(correctness.out_of_order_packets),
            grouped(correctness.unexpected_packets),
            grouped(correctness.payload_mismatches),
            grouped(correctness.discrepancy_count())
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_telemetry(output: &mut String, runs: &[RunData]) -> Result<()> {
    let summaries = runs
        .iter()
        .map(|run| {
            (
                scenario_label(run.result.scenario),
                TelemetrySummary::from_samples(run.samples.as_ref(), run.result.delivered_packets),
            )
        })
        .collect::<Vec<_>>();
    writeln!(output, "## Process telemetry\n")?;
    writeln!(
        output,
        "CPU averages are weighted by sample interval. CPU graphs require explicit milli-percent samples.\n"
    )?;
    writeln!(
        output,
        "| Scenario | Samples | Unavailable | Last sample | SFU ticks | RTC ticks |"
    )?;
    writeln!(output, "| --- | ---: | ---: | ---: | --- | --- |")?;
    for (label, summary) in &summaries {
        writeln!(
            output,
            "| {label} | {} | {} | {} | {} | {} |",
            summary.samples,
            summary.unavailable,
            summary
                .elapsed_ms
                .map_or_else(|| "n/a".to_owned(), format_milliseconds),
            observed(summary.server_ticks_observed),
            observed(summary.rtc_ticks_observed)
        )?;
    }
    writeln!(output)?;
    if runs.len() <= MAX_CHART_SCENARIOS {
        let chart_labels = runs
            .iter()
            .map(|run| chart_label(run.result.scenario))
            .collect::<Vec<_>>();
        render_cpu_overview(output, &summaries, &chart_labels)?;
    }
    render_metric_table(output, &summaries)?;
    render_telemetry_issues(output, runs)?;
    Ok(())
}

fn render_cpu_overview(
    output: &mut String,
    summaries: &[(String, TelemetrySummary)],
    labels: &[String],
) -> Result<()> {
    ensure!(
        summaries.len() == labels.len(),
        "CPU chart labels must match telemetry summaries"
    );
    let mut chart_labels = Vec::new();
    let mut omitted_labels = Vec::new();
    let mut averages = Vec::new();
    let mut peaks = Vec::new();
    for ((_label, summary), chart_label) in summaries.iter().zip(labels) {
        let (Some(average), Some(peak)) = (
            summary.server_cpu_percent_milli,
            summary.server_cpu_peak_percent_milli,
        ) else {
            omitted_labels.push(chart_label);
            continue;
        };
        chart_labels.push(chart_label.clone());
        averages.push(rounded_div(average, 1_000));
        peaks.push(rounded_div(peak, 1_000));
    }
    if !omitted_labels.is_empty() {
        writeln!(
            output,
            "CPU chart requires an interval-weighted average and sampled peak. Omitted scenarios: {}.\n",
            omitted_labels
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    if chart_labels.is_empty() {
        return Ok(());
    }
    render_category_charts(
        output,
        "SFU CPU average and peak",
        "The first line is the whole-window weighted average. The second line is the sampled peak.",
        &chart_labels,
        "CPU (%)",
        100,
        &[
            ChartSeries {
                name: "weighted average",
                values: &averages,
            },
            ChartSeries {
                name: "sampled peak",
                values: &peaks,
            },
        ],
    )
}

fn render_telemetry_issues(output: &mut String, runs: &[RunData]) -> Result<()> {
    let issues = runs.iter().filter_map(|run| {
        run.samples
            .as_ref()
            .filter(|sample_set| sample_set.unavailable > 0)
            .map(|sample_set| (run, sample_set))
    });
    if issues.clone().next().is_none() {
        return Ok(());
    }
    writeln!(output, "## Telemetry issues\n")?;
    writeln!(output, "| Scenario | Input | Unavailable | Error |")?;
    writeln!(output, "| --- | --- | ---: | --- |")?;
    for (run, sample_set) in issues {
        if sample_set.errors.is_empty() {
            writeln!(
                output,
                "| {} | {} | {} | unavailable telemetry record |",
                scenario_label(run.result.scenario),
                escape_table(&run.source),
                sample_set.unavailable
            )?;
            continue;
        }
        for error in &sample_set.errors {
            writeln!(
                output,
                "| {} | {} | {} | {} |",
                scenario_label(run.result.scenario),
                escape_table(&run.source),
                sample_set.unavailable,
                escape_table(error)
            )?;
        }
    }
    writeln!(output)?;
    Ok(())
}

fn render_metric_table(
    output: &mut String,
    summaries: &[(String, TelemetrySummary)],
) -> Result<()> {
    writeln!(
        output,
        "CPU and counter metrics cover the whole sample window including setup, warmup and drain.\n"
    )?;
    writeln!(
        output,
        "| Scenario | SFU CPU avg | SFU CPU peak | Efficiency | Forwarded | Sample egress | Loop heartbeat delay | SFU RSS | RTC CPU avg | RTC RSS |"
    )?;
    writeln!(
        output,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for (label, summary) in summaries {
        writeln!(
            output,
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            format_optional(summary.server_cpu_percent_milli, format_cpu_percent),
            format_optional(summary.server_cpu_peak_percent_milli, format_cpu_percent),
            format_optional(
                summary.deliveries_per_server_cpu_second,
                format_delivery_efficiency
            ),
            format_optional(
                summary.forwarded_packets_per_second,
                format_packets_per_second
            ),
            format_optional(
                summary.egress_payload_bits_per_second,
                format_bits_per_second
            ),
            format_packet_loop_health(summary),
            format_optional(summary.server_rss_bytes, format_mebibytes),
            format_optional(summary.rtc_cpu_percent_milli, format_cpu_percent),
            format_optional(summary.rtc_rss_bytes, format_mebibytes)
        )?;
    }
    writeln!(output)?;
    Ok(())
}

pub(crate) fn render_category_charts(
    output: &mut String,
    title: &str,
    description: &str,
    labels: &[String],
    y_axis: &str,
    minimum: u64,
    series: &[ChartSeries<'_>],
) -> Result<()> {
    ensure!(!labels.is_empty(), "chart labels cannot be empty");
    ensure!(!series.is_empty(), "chart series cannot be empty");
    ensure!(
        series.iter().all(|line| line.values.len() == labels.len()),
        "chart series lengths must match chart labels"
    );
    if labels.len() == 1 {
        writeln!(
            output,
            "The {title} chart is omitted because fewer than two scenarios have chartable values. The tabular metrics remain available.\n"
        )?;
        return Ok(());
    }
    if labels.len() > MAX_CHART_SCENARIOS {
        writeln!(
            output,
            "The {title} chart is omitted because it contains more than {MAX_CHART_SCENARIOS} scenarios. The tabular metrics remain available.\n"
        )?;
        return Ok(());
    }
    if series
        .iter()
        .any(|line| line.values.iter().any(|value| *value > MAX_MERMAID_INTEGER))
    {
        writeln!(
            output,
            "The {title} chart is omitted because a value exceeds Mermaid's exact-integer range. The tabular metrics remain available.\n"
        )?;
        return Ok(());
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < labels.len() {
        let remaining = labels.len() - start;
        let size = if remaining == CATEGORY_CHART_POINTS + 1 {
            CATEGORY_CHART_POINTS - 1
        } else {
            remaining.min(CATEGORY_CHART_POINTS)
        };
        ranges.push(start..start + size);
        start += size;
    }
    let chunk_count = ranges.len();
    if chunk_count > 1 {
        writeln!(
            output,
            "The {title} chart is split into {chunk_count} panels. Each panel uses its own y-axis scale.\n"
        )?;
    }
    for (chunk_index, range) in ranges.into_iter().enumerate() {
        let chart_labels = labels
            .get(range.clone())
            .context("chart label chunk is outside its labels")?;
        let chart_title = if chunk_count == 1 {
            title.to_owned()
        } else {
            format!("{title} ({}/{chunk_count})", chunk_index + 1)
        };
        let chart_series = series
            .iter()
            .map(|line| {
                let values = line
                    .values
                    .get(range.clone())
                    .context("chart series chunk is outside its values")?;
                Ok(ChartSeries {
                    name: line.name,
                    values,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let maximum = chart_axis_max(chart_series.iter().flat_map(|line| line.values), minimum);
        render_category_chart(
            output,
            &chart_title,
            description,
            chart_labels,
            y_axis,
            maximum,
            &chart_series,
        )?;
    }
    Ok(())
}

fn render_category_chart(
    output: &mut String,
    title: &str,
    description: &str,
    labels: &[String],
    y_axis: &str,
    maximum: u64,
    series: &[ChartSeries<'_>],
) -> Result<()> {
    ensure!(
        labels.len() >= 2,
        "category line chart needs at least two labels"
    );
    ensure!(
        series.len() <= LINE_COLORS.len(),
        "chart has more lines than its color palette"
    );
    writeln!(output, "{description}\n")?;
    write!(output, "Series colors: ")?;
    for (index, (line, (color, hex))) in series.iter().zip(LINE_COLORS).enumerate() {
        if index > 0 {
            write!(output, ". ")?;
        }
        write!(output, "{color} (`{hex}`) = {}", line.name)?;
    }
    writeln!(output, ".\n")?;
    writeln!(output, "```mermaid")?;
    writeln!(output, "---")?;
    writeln!(output, "config:")?;
    writeln!(output, "  themeVariables:")?;
    writeln!(output, "    xyChart:")?;
    let palette = LINE_COLORS
        .iter()
        .map(|(_name, hex)| *hex)
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(output, "      plotColorPalette: \"{palette}\"")?;
    writeln!(output, "---")?;
    writeln!(output, "xychart-beta")?;
    writeln!(output, "    accTitle: {title}")?;
    writeln!(output, "    accDescr: {description}")?;
    writeln!(output, "    title \"{title}\"")?;
    write!(output, "    x-axis [")?;
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            write!(output, ", ")?;
        }
        write!(output, "\"{label}\"")?;
    }
    writeln!(output, "]")?;
    writeln!(output, "    y-axis \"{y_axis}\" 0 --> {maximum}")?;
    for line in series {
        write!(output, "    line [")?;
        write_values(output, line.values)?;
        writeln!(output, "]")?;
    }
    writeln!(output, "```\n")?;
    Ok(())
}

fn write_values(output: &mut String, values: &[u64]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(output, ", ")?;
        }
        write!(output, "{value}")?;
    }
    Ok(())
}

fn chart_axis_max<'a>(values: impl Iterator<Item = &'a u64>, minimum: u64) -> u64 {
    let maximum = values.copied().max().unwrap_or_default();
    if maximum == 0 {
        return minimum.max(1);
    }
    if maximum <= minimum {
        return minimum;
    }
    let padded = (u128::from(maximum) * 11).div_ceil(10);
    let padded = u64::try_from(padded)
        .unwrap_or(MAX_MERMAID_INTEGER)
        .min(MAX_MERMAID_INTEGER);
    let step = 10_u64.pow(padded.ilog10().saturating_sub(1));
    padded
        .div_ceil(step)
        .saturating_mul(step)
        .min(MAX_MERMAID_INTEGER)
        .max(maximum)
        .max(minimum)
}

fn format_optional(value: Option<u64>, format: fn(u64) -> String) -> String {
    value.map_or_else(|| "n/a".to_owned(), format)
}

fn run_passed(run: &RunData) -> bool {
    validate_run(run).is_ok()
}

pub(crate) fn validate_run(run: &RunData) -> Result<()> {
    run.result.validate(run.result.scenario)
}

fn validation_label(run: &RunData) -> String {
    validate_run(run).map_or_else(
        |error| escape_table(&format!("{error:#}")),
        |()| "ok".to_owned(),
    )
}

fn telemetry_status(runs: &[RunData]) -> &'static str {
    if runs.iter().all(|run| run.samples.is_none()) {
        return "n/a";
    }
    if runs.iter().any(|run| {
        run.samples
            .as_ref()
            .is_none_or(|samples| samples.samples.is_empty() || samples.unavailable > 0)
    }) {
        return "INCOMPLETE";
    }
    let run_unresponsive = runs.iter().any(|run| {
        run.samples.as_ref().is_some_and(|samples| {
            samples
                .samples
                .iter()
                .any(|sample| sample.packet_loop_unresponsive)
        })
    });
    if run_unresponsive {
        "UNHEALTHY"
    } else {
        "COMPLETE"
    }
}

fn pacing_status(runs: &[RunData]) -> &'static str {
    if runs.is_empty() {
        "n/a"
    } else if runs.iter().all(|run| pacing_valid(&run.result)) {
        "VALID"
    } else {
        "INVALID"
    }
}

fn pacing_label(result: &ScenarioResult) -> &'static str {
    if pacing_valid(result) {
        "valid"
    } else {
        "INVALID"
    }
}

pub(crate) fn pacing_valid(result: &ScenarioResult) -> bool {
    let interval_ms = match result.scenario {
        ScenarioSpec::Smoke { .. }
        | ScenarioSpec::AudioMesh { .. }
        | ScenarioSpec::MixedConference { .. } => 20,
        ScenarioSpec::VideoGallery { .. } => 34,
    };
    result.max_send_lag_ms <= interval_ms
}

pub(crate) fn workload_matches(baseline: &RunData, comparison: &RunData) -> bool {
    let baseline = &baseline.result;
    let comparison = &comparison.result;
    baseline.schema_version == comparison.schema_version
        && baseline.profile == comparison.profile
        && baseline.scenario == comparison.scenario
        && baseline.server_policy == comparison.server_policy
        && baseline.plan == comparison.plan
        && baseline.offered_packets == comparison.offered_packets
        && baseline.offered_payload_bytes == comparison.offered_payload_bytes
}

pub(crate) fn delivery_rate(result: &ScenarioResult) -> u64 {
    result.achieved_deliveries_per_second()
}

pub(crate) fn scenario_key(spec: ScenarioSpec) -> (u8, u64, u64, u32, u32, u32, u32, u32) {
    let rank = match spec {
        ScenarioSpec::Smoke { .. } => 0,
        ScenarioSpec::AudioMesh { .. } => 1,
        ScenarioSpec::VideoGallery { .. } => 2,
        ScenarioSpec::MixedConference { .. } => 3,
    };
    let duration = u64::from(spec.duration_seconds()).max(1);
    let expected_deliveries = spec
        .plan()
        .map(|plan| plan.expected_deliveries)
        .unwrap_or_default();
    let deliveries_per_second = expected_deliveries / duration;
    (
        rank,
        deliveries_per_second,
        expected_deliveries,
        spec.room_count(),
        spec.peers_per_room(),
        spec.audio_publishers_per_room(),
        spec.video_publishers_per_room(),
        spec.duration_seconds(),
    )
}

pub(crate) fn scenario_label(spec: ScenarioSpec) -> String {
    match spec {
        ScenarioSpec::Smoke { receivers, packets } => {
            format!("smoke-{receivers}r-{packets}p")
        }
        ScenarioSpec::AudioMesh {
            rooms,
            peers,
            seconds,
        } => format!("audio-mesh-{rooms}x{peers}-{seconds}s"),
        ScenarioSpec::VideoGallery {
            rooms,
            peers,
            publishers,
            seconds,
        } => format!("video-gallery-{rooms}x{peers}-{publishers}p-{seconds}s"),
        ScenarioSpec::MixedConference {
            rooms,
            peers,
            audio_publishers,
            video_publishers,
            seconds,
        } => format!(
            "mixed-conference-{rooms}x{peers}-{audio_publishers}a-{video_publishers}v-{seconds}s"
        ),
    }
}

fn publisher_label(spec: ScenarioSpec) -> String {
    match (
        spec.audio_publishers_per_room(),
        spec.video_publishers_per_room(),
    ) {
        (audio, 0) => format!("{audio} audio"),
        (0, video) => format!("{video} video"),
        (audio, video) => format!("{audio} audio / {video} video"),
    }
}

pub(crate) fn chart_label(spec: ScenarioSpec) -> String {
    match spec {
        ScenarioSpec::Smoke { receivers, packets } => {
            format!("S {receivers}r/{packets}p")
        }
        ScenarioSpec::AudioMesh {
            rooms,
            peers,
            seconds,
        } => format!("A {rooms}x{peers}/{seconds}s"),
        ScenarioSpec::VideoGallery {
            rooms,
            peers,
            publishers,
            seconds,
        } => format!("V {rooms}x{peers}/{publishers}p/{seconds}s"),
        ScenarioSpec::MixedConference {
            rooms,
            peers,
            audio_publishers,
            video_publishers,
            seconds,
        } => format!("M {rooms}x{peers}/{audio_publishers}a/{video_publishers}v/{seconds}s"),
    }
}

fn revision_label(runs: &[RunData]) -> String {
    let revisions = runs
        .iter()
        .filter_map(|run| run.result.o_sfu_revision.as_deref())
        .collect::<BTreeSet<_>>();
    let missing = runs.iter().any(|run| run.result.o_sfu_revision.is_none());
    if revisions.len() == 1 && !missing {
        revisions
            .first()
            .map_or_else(|| "n/a".to_owned(), ToString::to_string)
    } else if revisions.is_empty() {
        "n/a".to_owned()
    } else {
        "mixed".to_owned()
    }
}

fn scheduled_payload_bits_per_second(result: &ScenarioResult) -> u64 {
    payload_bits_per_second(
        result.plan.offered_payload_bytes,
        planned_duration_ms(result.scenario),
    )
}

fn planned_duration_ms(scenario: ScenarioSpec) -> u64 {
    match scenario {
        ScenarioSpec::Smoke { packets, .. } => {
            u64::from(packets) * 1_000 / u64::from(AUDIO_PACKETS_PER_SECOND)
        }
        ScenarioSpec::AudioMesh { seconds, .. }
        | ScenarioSpec::VideoGallery { seconds, .. }
        | ScenarioSpec::MixedConference { seconds, .. } => u64::from(seconds) * 1_000,
    }
}

pub(crate) fn delivered_payload_bits_per_second(result: &ScenarioResult) -> u64 {
    payload_bits_per_second(result.delivered_payload_bytes, result.elapsed_ms)
}

fn payload_bits_per_second(bytes: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        return 0;
    }
    let rate = u128::from(bytes) * 8_000 / u128::from(elapsed_ms);
    u64::try_from(rate).unwrap_or(u64::MAX)
}

fn payload_rate_for_profile(packet_count: u64, payload_bytes: usize, seconds: u32) -> Result<u64> {
    ensure!(seconds > 0, "media profile duration must be positive");
    let bits = packet_count
        .checked_mul(u64::try_from(payload_bytes).context("payload size exceeds u64")?)
        .and_then(|bytes| bytes.checked_mul(8))
        .context("media profile bitrate overflowed")?;
    ensure!(
        bits.is_multiple_of(u64::from(seconds)),
        "media profile bitrate is not an exact integer"
    );
    Ok(bits / u64::from(seconds))
}

fn format_profile_packet_rate(packet_count: u64, seconds: u32) -> Result<String> {
    ensure!(seconds > 0, "media profile duration must be positive");
    let tenths = u128::from(packet_count)
        .checked_mul(10)
        .context("media profile packet rate overflowed")?;
    ensure!(
        tenths.is_multiple_of(u128::from(seconds)),
        "media profile packet rate needs more than one decimal"
    );
    let tenths = tenths / u128::from(seconds);
    if tenths.is_multiple_of(10) {
        Ok(grouped(u64::try_from(tenths / 10).unwrap_or(u64::MAX)))
    } else {
        Ok(format!("{}.{:01}", tenths / 10, tenths % 10))
    }
}

fn rounded_div(value: u64, divisor: u64) -> u64 {
    let rounded = (u128::from(value) + u128::from(divisor) / 2) / u128::from(divisor);
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(digit);
    }
    output
}

pub(crate) fn format_cpu_percent(value: u64) -> String {
    format!("{}.{:03}%", value / 1_000, value % 1_000)
}

pub(crate) fn format_mebibytes(value: u64) -> String {
    const MEBIBYTE: u128 = 1024 * 1024;
    let tenths = u128::from(value) * 10 / MEBIBYTE;
    format!("{}.{:01} MiB", tenths / 10, tenths % 10)
}

pub(crate) fn format_milliseconds(value: u64) -> String {
    format!("{} ms", grouped(value))
}

fn format_delivery_efficiency(value: u64) -> String {
    format!("{} deliveries/CPU-s", grouped(value))
}

fn format_packets_per_second(value: u64) -> String {
    format!("{} packets/s", grouped(value))
}

pub(crate) fn format_bits_per_second(value: u64) -> String {
    if value >= 1_000_000_000 {
        return format_decimal_rate(value, 1_000_000_000, "Gbit/s");
    }
    if value >= 1_000_000 {
        return format_decimal_rate(value, 1_000_000, "Mbit/s");
    }
    if value >= 1_000 {
        return format_decimal_rate(value, 1_000, "kbit/s");
    }
    format!("{} bit/s", grouped(value))
}

fn format_decimal_rate(value: u64, unit: u64, suffix: &str) -> String {
    let tenths = u128::from(value) * 10 / u128::from(unit);
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
}

const fn observed(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(crate) fn escape_table(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '|' => output.push_str("&#124;"),
            '\\' => output.push_str("&#92;"),
            '[' => output.push_str("&#91;"),
            ']' => output.push_str("&#93;"),
            '(' => output.push_str("&#40;"),
            ')' => output.push_str("&#41;"),
            '!' => output.push_str("&#33;"),
            '*' => output.push_str("&#42;"),
            '_' => output.push_str("&#95;"),
            '`' => output.push_str("&#96;"),
            '~' => output.push_str("&#126;"),
            '\r' | '\n' => output.push(' '),
            _ => output.push(character),
        }
    }
    output
}

pub(crate) fn validate_artifact_url(artifact_url: Option<&str>) -> Result<()> {
    if let Some(url) = artifact_url {
        let parts = url
            .strip_prefix("https://github.com/")
            .map(|path| path.split('/').collect::<Vec<_>>());
        let valid = parts.as_deref().is_some_and(|parts| {
            let [
                owner,
                repository,
                "actions",
                "runs",
                run_id,
                "artifacts",
                artifact_id,
            ] = parts
            else {
                return false;
            };
            valid_repository_component(owner)
                && valid_repository_component(repository)
                && numeric_identifier(run_id)
                && numeric_identifier(artifact_id)
        });
        ensure!(
            valid,
            "artifact URL must identify one GitHub Actions artifact"
        );
    }
    Ok(())
}

pub(crate) fn validate_flamegraph_url(flamegraph_url: Option<&str>) -> Result<()> {
    if let Some(url) = flamegraph_url {
        let parts = url
            .strip_prefix("https://github.com/")
            .map(|path| path.split('/').collect::<Vec<_>>());
        let valid = parts.as_deref().is_some_and(|parts| {
            let [
                owner,
                repository,
                "releases",
                "download",
                "load-test-assets",
                asset,
            ] = parts
            else {
                return false;
            };
            valid_repository_component(owner)
                && valid_repository_component(repository)
                && valid_flamegraph_asset(asset)
        });
        ensure!(
            valid,
            "flamegraph URL must identify one published load-test PNG"
        );
    }
    Ok(())
}

fn valid_flamegraph_asset(value: &str) -> bool {
    let Some(ids) = value
        .strip_prefix("o-sfu-flamegraph-")
        .and_then(|value| value.strip_suffix(".png"))
    else {
        return false;
    };
    let ids = ids
        .strip_prefix("baseline-")
        .or_else(|| ids.strip_prefix("comparison-"))
        .unwrap_or(ids);
    let mut parts = ids.split('-');
    let (Some(run_id), Some(attempt), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    numeric_identifier(run_id) && numeric_identifier(attempt)
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn numeric_identifier(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn ensure_summary_size(summary: &str) -> Result<()> {
    ensure!(
        summary.len() <= GITHUB_SUMMARY_LIMIT_BYTES,
        "report exceeds GitHub's one MiB job-summary limit"
    );
    Ok(())
}

#[cfg(test)]
#[path = "TESTS/report_tests.rs"]
mod tests;
