use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::ErrorKind,
    iter::repeat_n,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::{ScenarioResult, ScenarioSpec};

const BAR_WIDTH: usize = 32;
const BAR_WIDTH_U128: u128 = 32;
const CPU_TIMELINE_WIDTH: usize = 48;
const GITHUB_SUMMARY_LIMIT_BYTES: usize = 1024 * 1024;
const RESULT_LIMIT_BYTES: u64 = 1024 * 1024;
const SAMPLES_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INPUTS: usize = 256;
const MAX_TELEMETRY_SAMPLES: usize = 10_000;
const MAX_TELEMETRY_ERRORS: usize = 8;

#[derive(Clone)]
struct RunData {
    source: String,
    result: ScenarioResult,
    samples: Option<SampleSet>,
}

struct LoadFailure {
    source: String,
    error: String,
}

#[derive(Clone)]
struct SampleSet {
    samples: Vec<TelemetrySample>,
    unavailable: usize,
    errors: Vec<String>,
}

#[derive(Clone, Copy)]
struct TelemetrySample {
    elapsed_ms: u64,
    clock_ticks_per_second: Option<u64>,
    server_cpu_ticks: Option<u64>,
    server_start_time_ticks: Option<u64>,
    server_rss_bytes: Option<u64>,
    rtc_cpu_ticks: Option<u64>,
    rtc_rss_bytes: Option<u64>,
    server_cpu_percent_milli: Option<u64>,
    rtc_cpu_percent_milli: Option<u64>,
    forwarded_packets: Option<u64>,
    egress_payload_bytes: Option<u64>,
    packet_loop_delay_ms: Option<u64>,
}

struct TelemetrySummary {
    samples: usize,
    unavailable: usize,
    elapsed_ms: Option<u64>,
    server_ticks_observed: bool,
    rtc_ticks_observed: bool,
    server_cpu_percent_milli: Option<u64>,
    server_rss_bytes: Option<u64>,
    rtc_cpu_percent_milli: Option<u64>,
    rtc_rss_bytes: Option<u64>,
    server_cpu_time_ms: Option<u64>,
    deliveries_per_server_cpu_second: Option<u64>,
    forwarded_packets_per_second: Option<u64>,
    egress_payload_bits_per_second: Option<u64>,
    packet_loop_delay_ms: Option<u64>,
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
pub fn write(inputs: &[PathBuf], output: &Path, artifact_url: Option<&str>) -> Result<()> {
    let summary = render(inputs, artifact_url)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("failed to create the report directory")?;
    }
    fs::write(output, summary).context("failed to write the report")
}

fn load_run(input: &Path) -> Result<RunData> {
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

fn parse_samples(payload: &str) -> SampleSet {
    let mut samples = Vec::new();
    let mut unavailable = 0_usize;
    let mut errors = Vec::new();
    for line in payload
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if samples.len() >= MAX_TELEMETRY_SAMPLES {
            unavailable = unavailable.saturating_add(1);
            retain_error(&mut errors, "telemetry sample limit reached".to_owned());
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                if let Some(sample) = TelemetrySample::from_value(&value) {
                    samples.push(sample);
                } else {
                    unavailable = unavailable.saturating_add(1);
                    retain_error(&mut errors, telemetry_error(&value));
                }
            }
            Err(error) => {
                unavailable = unavailable.saturating_add(1);
                retain_error(&mut errors, format!("malformed telemetry record: {error}"));
            }
        }
    }
    samples.sort_unstable_by_key(|sample| sample.elapsed_ms);
    SampleSet {
        samples,
        unavailable,
        errors,
    }
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
    fn from_value(value: &Value) -> Option<Self> {
        if value.get("status").and_then(Value::as_str) == Some("error") {
            return None;
        }
        let sample = Self {
            elapsed_ms: value.get("elapsedMs")?.as_u64()?,
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
            forwarded_packets: value
                .get("traffic")
                .and_then(|traffic| traffic.get("forwardedLocalRtc"))
                .and_then(|forwarded| forwarded.get("packets"))
                .and_then(Value::as_u64),
            egress_payload_bytes: value
                .get("traffic")
                .and_then(|traffic| traffic.get("egress"))
                .and_then(|egress| egress.get("payloadBytes"))
                .and_then(Value::as_u64),
            packet_loop_delay_ms: value.get("workers").and_then(Value::as_array).and_then(
                |workers| {
                    workers
                        .iter()
                        .filter_map(|worker| worker.get("packetLoopDelayMs"))
                        .filter_map(Value::as_u64)
                        .max()
                },
            ),
        };
        (sample.server_cpu_ticks.is_some()
            || sample.server_rss_bytes.is_some()
            || sample.rtc_cpu_ticks.is_some()
            || sample.rtc_rss_bytes.is_some()
            || sample.server_cpu_percent_milli.is_some()
            || sample.rtc_cpu_percent_milli.is_some()
            || sample.forwarded_packets.is_some()
            || sample.egress_payload_bytes.is_some()
            || sample.packet_loop_delay_ms.is_some())
        .then_some(sample)
    }
}

impl TelemetrySummary {
    fn from_samples(sample_set: Option<&SampleSet>, delivered_packets: u64) -> Self {
        let Some(sample_set) = sample_set else {
            return Self {
                samples: 0,
                unavailable: 0,
                elapsed_ms: None,
                server_ticks_observed: false,
                rtc_ticks_observed: false,
                server_cpu_percent_milli: None,
                server_rss_bytes: None,
                rtc_cpu_percent_milli: None,
                rtc_rss_bytes: None,
                server_cpu_time_ms: None,
                deliveries_per_server_cpu_second: None,
                forwarded_packets_per_second: None,
                egress_payload_bits_per_second: None,
                packet_loop_delay_ms: None,
            };
        };
        let samples = &sample_set.samples;
        let server_cpu_ticks = server_cpu_ticks(samples);
        Self {
            samples: samples.len(),
            unavailable: sample_set.unavailable,
            elapsed_ms: samples.last().map(|sample| sample.elapsed_ms),
            server_ticks_observed: samples
                .iter()
                .any(|sample| sample.server_cpu_ticks.is_some()),
            rtc_ticks_observed: samples.iter().any(|sample| sample.rtc_cpu_ticks.is_some()),
            server_cpu_percent_milli: weighted_average(samples, |sample| {
                sample.server_cpu_percent_milli
            }),
            server_rss_bytes: samples
                .iter()
                .filter_map(|sample| sample.server_rss_bytes)
                .max(),
            rtc_cpu_percent_milli: weighted_average(samples, |sample| sample.rtc_cpu_percent_milli),
            rtc_rss_bytes: samples
                .iter()
                .filter_map(|sample| sample.rtc_rss_bytes)
                .max(),
            server_cpu_time_ms: server_cpu_ticks.map(|(ticks, ticks_per_second)| {
                let milliseconds = u128::from(ticks) * 1_000 / u128::from(ticks_per_second);
                u64::try_from(milliseconds).unwrap_or(u64::MAX)
            }),
            deliveries_per_server_cpu_second: server_cpu_ticks.map(|(ticks, ticks_per_second)| {
                let deliveries = u128::from(delivered_packets) * u128::from(ticks_per_second)
                    / u128::from(ticks);
                u64::try_from(deliveries).unwrap_or(u64::MAX)
            }),
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
        }
    }
}

fn nested_u64(value: &Value, object: &str, field: &str) -> Option<u64> {
    value.get(object)?.get(field)?.as_u64()
}

fn weighted_average(
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
) -> Option<u64> {
    let mut previous_elapsed_ms = None;
    let mut weighted_sum = 0_u128;
    let mut total_weight = 0_u128;
    for sample in samples {
        if let Some(previous) = previous_elapsed_ms {
            let weight = sample.elapsed_ms.saturating_sub(previous);
            if weight > 0
                && let Some(value) = value(sample)
            {
                weighted_sum = weighted_sum.saturating_add(u128::from(value) * u128::from(weight));
                total_weight = total_weight.saturating_add(u128::from(weight));
            }
        }
        previous_elapsed_ms = Some(sample.elapsed_ms);
    }
    (total_weight > 0)
        .then(|| weighted_sum / total_weight)
        .and_then(|average| u64::try_from(average).ok())
}

fn server_cpu_ticks(samples: &[TelemetrySample]) -> Option<(u64, u64)> {
    let first = samples.iter().find_map(|sample| {
        Some((
            sample.server_cpu_ticks?,
            sample.server_start_time_ticks?,
            sample.clock_ticks_per_second?,
        ))
    })?;
    let last = samples.iter().rev().find_map(|sample| {
        let ticks = sample.server_cpu_ticks?;
        let start_time = sample.server_start_time_ticks?;
        let ticks_per_second = sample.clock_ticks_per_second?;
        (start_time == first.1 && ticks_per_second == first.2).then_some((ticks, ticks_per_second))
    })?;
    let delta = last.0.checked_sub(first.0)?;
    (delta > 0 && last.1 > 0).then_some((delta, last.1))
}

fn counter_rate(
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
    scale: u64,
) -> Option<u64> {
    let first = samples
        .iter()
        .find_map(|sample| value(sample).map(|value| (sample.elapsed_ms, value)))?;
    let last = samples
        .iter()
        .rev()
        .find_map(|sample| value(sample).map(|value| (sample.elapsed_ms, value)))?;
    let elapsed_ms = last.0.checked_sub(first.0)?;
    let delta = last.1.checked_sub(first.1)?;
    if elapsed_ms == 0 {
        return None;
    }
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
    let passed = !runs.is_empty() && failures.is_empty() && runs.iter().all(run_passed);
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
        if passed { "PASS" } else { "FAIL" },
        runs.len(),
        failures.len(),
        escape_table(&revision)
    )?;
    writeln!(
        output,
        "Performance samples: **{}**. A sample is invalid when send lag exceeds one media interval.\n",
        pacing_status(runs)
    )?;
    if let Some(url) = artifact_url {
        writeln!(output, "[Download raw results and logs]({url})\n")?;
    }
    Ok(())
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

fn render_workloads(output: &mut String, runs: &[RunData]) -> Result<()> {
    writeln!(output, "## Workloads\n")?;
    writeln!(
        output,
        "| Exact | Scenario | Rooms | Peers/room | Publishers/room | Streams | Routes | Offered packets | Expected deliveries | Duration | Max send lag | Pacing | Profile | Validation |"
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
            scenario.publishers_per_room(),
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

fn render_delivery(output: &mut String, runs: &[RunData]) -> Result<()> {
    writeln!(output, "## Exact delivery\n")?;
    writeln!(
        output,
        "Every bar uses one common scale. Exact values remain authoritative.\n"
    )?;
    let maximum = runs
        .iter()
        .flat_map(|run| {
            [
                run.result.plan.expected_deliveries,
                run.result.delivered_packets,
            ]
        })
        .max()
        .unwrap_or_default();
    let label_width = runs
        .iter()
        .map(|run| scenario_label(run.result.scenario).len())
        .max()
        .unwrap_or_default();
    writeln!(output, "```text")?;
    for run in runs {
        let result = &run.result;
        let label = scenario_label(result.scenario);
        writeln!(
            output,
            "{label:label_width$} expected  [{}] {}",
            bar(result.plan.expected_deliveries, maximum, '='),
            grouped(result.plan.expected_deliveries)
        )?;
        writeln!(
            output,
            "{:label_width$} delivered [{}] {}  {}/s",
            "",
            bar(result.delivered_packets, maximum, '#'),
            grouped(result.delivered_packets),
            grouped(delivery_rate(result))
        )?;
    }
    writeln!(output, "```\n")?;
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
    render_cpu_timeline(output, runs)?;
    render_metric(
        output,
        "SFU CPU average",
        &summaries,
        |summary| summary.server_cpu_percent_milli,
        format_cpu_percent,
    )?;
    render_metric(
        output,
        "SFU CPU time",
        &summaries,
        |summary| summary.server_cpu_time_ms,
        format_milliseconds,
    )?;
    render_metric(
        output,
        "SFU delivery efficiency",
        &summaries,
        |summary| summary.deliveries_per_server_cpu_second,
        format_delivery_efficiency,
    )?;
    render_metric(
        output,
        "SFU forwarded packets",
        &summaries,
        |summary| summary.forwarded_packets_per_second,
        format_packets_per_second,
    )?;
    render_metric(
        output,
        "SFU payload egress",
        &summaries,
        |summary| summary.egress_payload_bits_per_second,
        format_bits_per_second,
    )?;
    render_metric(
        output,
        "Packet-loop delay peak",
        &summaries,
        |summary| summary.packet_loop_delay_ms,
        format_milliseconds,
    )?;
    render_metric(
        output,
        "SFU RSS peak",
        &summaries,
        |summary| summary.server_rss_bytes,
        format_mebibytes,
    )?;
    render_metric(
        output,
        "RTC CPU average",
        &summaries,
        |summary| summary.rtc_cpu_percent_milli,
        format_cpu_percent,
    )?;
    render_metric(
        output,
        "RTC RSS peak",
        &summaries,
        |summary| summary.rtc_rss_bytes,
        format_mebibytes,
    )?;
    render_telemetry_issues(output, runs)?;
    Ok(())
}

fn render_cpu_timeline(output: &mut String, runs: &[RunData]) -> Result<()> {
    let maximum = runs
        .iter()
        .filter_map(|run| run.samples.as_ref())
        .flat_map(|sample_set| sample_set.samples.iter())
        .filter_map(|sample| sample.server_cpu_percent_milli)
        .max()
        .unwrap_or_default();
    let label_width = runs
        .iter()
        .map(|run| scenario_label(run.result.scenario).len())
        .max()
        .unwrap_or_default();
    writeln!(
        output,
        "SFU CPU timeline, common peak scale {}",
        format_cpu_percent(maximum)
    )?;
    writeln!(output, "```text")?;
    for run in runs {
        let label = scenario_label(run.result.scenario);
        let timeline = run
            .samples
            .as_ref()
            .and_then(|sample_set| cpu_timeline(&sample_set.samples, maximum));
        writeln!(
            output,
            "{label:label_width$} {}",
            timeline.as_deref().unwrap_or("n/a")
        )?;
    }
    writeln!(output, "```\n")?;
    Ok(())
}

fn cpu_timeline(samples: &[TelemetrySample], maximum: u64) -> Option<String> {
    let values = samples
        .iter()
        .filter_map(|sample| sample.server_cpu_percent_milli)
        .collect::<Vec<_>>();
    let buckets = values.len().min(CPU_TIMELINE_WIDTH);
    if buckets == 0 {
        return None;
    }
    let mut timeline = String::with_capacity(buckets);
    for bucket in 0..buckets {
        let start = bucket * values.len() / buckets;
        let end = (bucket + 1) * values.len() / buckets;
        let peak = values
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .copied()
            .max()
            .unwrap_or_default();
        timeline.push(cpu_level(peak, maximum));
    }
    Some(timeline)
}

fn cpu_level(value: u64, maximum: u64) -> char {
    const LEVELS: &[u8] = b".:-=+*#%@";
    if maximum == 0 {
        return '.';
    }
    let highest = LEVELS.len().saturating_sub(1);
    let scaled =
        u128::from(value) * u128::try_from(highest).unwrap_or_default() / u128::from(maximum);
    let index = usize::try_from(scaled).unwrap_or(highest).min(highest);
    char::from(LEVELS.get(index).copied().unwrap_or(b'@'))
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

fn render_metric(
    output: &mut String,
    title: &str,
    summaries: &[(String, TelemetrySummary)],
    value: fn(&TelemetrySummary) -> Option<u64>,
    format: fn(u64) -> String,
) -> Result<()> {
    let maximum = summaries
        .iter()
        .filter_map(|(_label, summary)| value(summary))
        .max()
        .unwrap_or_default();
    let label_width = summaries
        .iter()
        .map(|(label, _summary)| label.len())
        .max()
        .unwrap_or_default();
    writeln!(output, "{title}")?;
    writeln!(output, "```text")?;
    for (label, summary) in summaries {
        if let Some(metric) = value(summary) {
            writeln!(
                output,
                "{label:label_width$} [{}] {}",
                bar(metric, maximum, '#'),
                format(metric)
            )?;
        } else {
            writeln!(
                output,
                "{label:label_width$} [{}] n/a",
                ".".repeat(BAR_WIDTH)
            )?;
        }
    }
    writeln!(output, "```\n")?;
    Ok(())
}

fn run_passed(run: &RunData) -> bool {
    validate_run(run).is_ok()
}

fn validate_run(run: &RunData) -> Result<()> {
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
        "INCOMPLETE"
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

fn pacing_valid(result: &ScenarioResult) -> bool {
    let interval_ms = match result.scenario {
        ScenarioSpec::Smoke { .. } | ScenarioSpec::AudioMesh { .. } => 20,
        ScenarioSpec::VideoGallery { .. } => 34,
    };
    result.max_send_lag_ms <= interval_ms
}

fn delivery_rate(result: &ScenarioResult) -> u64 {
    result.achieved_deliveries_per_second()
}

fn scenario_key(spec: ScenarioSpec) -> (u8, u32, u32, u32, u32, u64) {
    let rank = match spec {
        ScenarioSpec::Smoke { .. } => 0,
        ScenarioSpec::AudioMesh { .. } => 1,
        ScenarioSpec::VideoGallery { .. } => 2,
    };
    (
        rank,
        spec.room_count(),
        spec.peers_per_room(),
        spec.publishers_per_room(),
        spec.duration_seconds(),
        spec.plan()
            .map(|plan| plan.expected_deliveries)
            .unwrap_or_default(),
    )
}

fn scenario_label(spec: ScenarioSpec) -> String {
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

fn bar(value: u64, maximum: u64, fill: char) -> String {
    let cells = if value == 0 || maximum == 0 {
        0
    } else {
        let scaled =
            (u128::from(value) * BAR_WIDTH_U128 + u128::from(maximum) / 2) / u128::from(maximum);
        usize::try_from(scaled)
            .unwrap_or(BAR_WIDTH)
            .clamp(1, BAR_WIDTH)
    };
    let mut output = String::with_capacity(BAR_WIDTH);
    output.extend(repeat_n(fill, cells));
    output.extend(repeat_n('.', BAR_WIDTH - cells));
    output
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

fn format_cpu_percent(value: u64) -> String {
    format!("{}.{:03}%", value / 1_000, value % 1_000)
}

fn format_mebibytes(value: u64) -> String {
    const MEBIBYTE: u128 = 1024 * 1024;
    let tenths = u128::from(value) * 10 / MEBIBYTE;
    format!("{}.{:01} MiB", tenths / 10, tenths % 10)
}

fn format_milliseconds(value: u64) -> String {
    format!("{} ms", grouped(value))
}

fn format_delivery_efficiency(value: u64) -> String {
    format!("{} deliveries/CPU-s", grouped(value))
}

fn format_packets_per_second(value: u64) -> String {
    format!("{} packets/s", grouped(value))
}

fn format_bits_per_second(value: u64) -> String {
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

fn escape_table(value: &str) -> String {
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

fn validate_artifact_url(artifact_url: Option<&str>) -> Result<()> {
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

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn numeric_identifier(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn ensure_summary_size(summary: &str) -> Result<()> {
    ensure!(
        summary.len() <= GITHUB_SUMMARY_LIMIT_BYTES,
        "report exceeds GitHub's one MiB job-summary limit"
    );
    Ok(())
}

#[cfg(test)]
#[path = "TESTS/report_tests.rs"]
mod tests;
