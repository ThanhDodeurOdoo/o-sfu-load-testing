use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::report::{
    ChartSeries, LoadFailure, MAX_INPUTS, RunData, TelemetrySummary, chart_label,
    delivered_payload_bits_per_second, delivery_rate, ensure_summary_size, escape_table,
    format_bits_per_second, format_cpu_percent, format_mebibytes, format_milliseconds, load_run,
    pacing_valid, render_category_charts, render_consumer_context, render_media_profile,
    render_scenario_legend, scenario_key, scenario_label, validate_artifact_url, validate_run,
};

struct Side {
    runs: Vec<RunData>,
    failures: Vec<LoadFailure>,
}

#[derive(Clone, Copy)]
struct RunPair<'a> {
    baseline: &'a RunData,
    comparison: &'a RunData,
    workload_matches: bool,
}

struct Pairing<'a> {
    pairs: Vec<RunPair<'a>>,
    issues: Vec<String>,
}

struct PairMetrics<'a> {
    pair: RunPair<'a>,
    baseline: TelemetrySummary,
    comparison: TelemetrySummary,
}

struct Revision {
    label: String,
    valid: bool,
}

struct Status<'a> {
    baseline: &'a Side,
    comparison: &'a Side,
    baseline_revision: &'a Revision,
    comparison_revision: &'a Revision,
    workloads_match: bool,
    exact: bool,
    performance_valid: bool,
    artifact_url: Option<&'a str>,
}

/// Renders a paired o-sfu revision comparison.
///
/// Invalid or missing result inputs remain visible in the rendered report.
///
/// # Errors
///
/// Returns an error when either side has no inputs, an input limit is exceeded,
/// the artifact URL is unsafe for Markdown or the summary exceeds one MiB.
pub fn render(
    baseline_inputs: &[PathBuf],
    comparison_inputs: &[PathBuf],
    artifact_url: Option<&str>,
) -> Result<String> {
    ensure!(!baseline_inputs.is_empty(), "baseline inputs are required");
    ensure!(
        !comparison_inputs.is_empty(),
        "comparison inputs are required"
    );
    ensure!(
        baseline_inputs.len() <= MAX_INPUTS,
        "at most 256 baseline inputs are allowed"
    );
    ensure!(
        comparison_inputs.len() <= MAX_INPUTS,
        "at most 256 comparison inputs are allowed"
    );
    validate_artifact_url(artifact_url)?;
    let baseline = load_side(baseline_inputs);
    let comparison = load_side(comparison_inputs);
    render_sides(&baseline, &comparison, artifact_url)
}

/// Writes a paired o-sfu revision comparison to `output`.
///
/// # Errors
///
/// Returns an error when rendering, directory creation or persistence fails.
pub fn write(
    baseline_inputs: &[PathBuf],
    comparison_inputs: &[PathBuf],
    output: &Path,
    artifact_url: Option<&str>,
) -> Result<()> {
    let summary = render(baseline_inputs, comparison_inputs, artifact_url)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("failed to create the comparison report directory")?;
    }
    fs::write(output, summary).context("failed to write the comparison report")
}

fn load_side(inputs: &[PathBuf]) -> Side {
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
    runs.sort_unstable_by(|left, right| {
        scenario_key(left.result.scenario)
            .cmp(&scenario_key(right.result.scenario))
            .then_with(|| left.source.cmp(&right.source))
    });
    failures.sort_unstable_by(|left, right| left.source.cmp(&right.source));
    Side { runs, failures }
}

fn render_sides(baseline: &Side, comparison: &Side, artifact_url: Option<&str>) -> Result<String> {
    let baseline_revision = revision(&baseline.runs);
    let comparison_revision = revision(&comparison.runs);
    let pairing = pair_runs(&baseline.runs, &comparison.runs);
    let metrics = pairing
        .pairs
        .iter()
        .copied()
        .map(|pair| PairMetrics {
            baseline: telemetry(pair.baseline),
            comparison: telemetry(pair.comparison),
            pair,
        })
        .collect::<Vec<_>>();
    let revisions_valid = baseline_revision.valid
        && comparison_revision.valid
        && baseline_revision.label != comparison_revision.label;
    let workloads_match = !pairing.pairs.is_empty()
        && pairing.issues.is_empty()
        && pairing.pairs.iter().all(|pair| pair.workload_matches);
    let exact = workloads_match
        && baseline.failures.is_empty()
        && comparison.failures.is_empty()
        && pairing.pairs.iter().all(|pair| {
            validate_run(pair.baseline).is_ok() && validate_run(pair.comparison).is_ok()
        });
    let performance_valid = revisions_valid && exact && metrics.iter().all(pair_performance_valid);
    let mut output = String::new();
    writeln!(output, "# o-sfu revision comparison\n")?;
    render_status(
        &mut output,
        &Status {
            baseline,
            comparison,
            baseline_revision: &baseline_revision,
            comparison_revision: &comparison_revision,
            workloads_match,
            exact,
            performance_valid,
            artifact_url,
        },
    )?;
    render_issues(
        &mut output,
        baseline,
        comparison,
        &baseline_revision,
        &comparison_revision,
        &pairing,
    )?;
    if pairing.pairs.is_empty() {
        writeln!(output, "No scenario pair was available for comparison.\n")?;
    } else {
        render_workload_identity(&mut output, &pairing.pairs)?;
        render_media_profile(&mut output)?;
        render_scenario_legend(&mut output)?;
        render_exact_delivery(&mut output, &pairing.pairs)?;
        render_graphs(
            &mut output,
            &metrics,
            &baseline_revision,
            &comparison_revision,
        )?;
        render_performance_table(&mut output, &metrics)?;
        render_resource_table(&mut output, &metrics)?;
    }
    ensure_summary_size(&output)?;
    Ok(output)
}

fn render_status(output: &mut String, status: &Status<'_>) -> Result<()> {
    let revisions_valid = status.baseline_revision.valid
        && status.comparison_revision.valid
        && status.baseline_revision.label != status.comparison_revision.label;
    writeln!(
        output,
        "| Exact work | Workloads | Revisions | Performance samples | Scenarios | Failed inputs | Baseline revision | Comparison revision |"
    )?;
    writeln!(
        output,
        "| --- | --- | --- | --- | ---: | ---: | --- | --- |"
    )?;
    writeln!(
        output,
        "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
        pass_fail(status.exact),
        if status.workloads_match {
            "IDENTICAL"
        } else {
            "MISMATCH"
        },
        if revisions_valid { "VALID" } else { "INVALID" },
        if status.performance_valid {
            "VALID"
        } else {
            "INVALID"
        },
        status.baseline.runs.len().max(status.comparison.runs.len()),
        status.baseline.failures.len() + status.comparison.failures.len(),
        escape_table(&status.baseline_revision.label),
        escape_table(&status.comparison_revision.label)
    )?;
    writeln!(
        output,
        "Every delta is comparison minus baseline. CPU time covers setup, warmup, measured work and drain.\n"
    )?;
    writeln!(
        output,
        "Both revisions use the same hosted runner and CPU affinity. Timing and resource deltas remain trend evidence until a dedicated runner provides a controlled testbed.\n"
    )?;
    if let Some(url) = status.artifact_url {
        writeln!(
            output,
            "[Download both revisions, telemetry and logs]({url})\n"
        )?;
    }
    Ok(())
}

fn render_issues(
    output: &mut String,
    baseline: &Side,
    comparison: &Side,
    baseline_revision: &Revision,
    comparison_revision: &Revision,
    pairing: &Pairing<'_>,
) -> Result<()> {
    let revision_issue = !baseline_revision.valid
        || !comparison_revision.valid
        || baseline_revision.label == comparison_revision.label;
    if baseline.failures.is_empty()
        && comparison.failures.is_empty()
        && pairing.issues.is_empty()
        && !revision_issue
    {
        return Ok(());
    }
    writeln!(output, "## Comparison issues\n")?;
    for failure in &baseline.failures {
        writeln!(
            output,
            "- Baseline input `{}`: {}",
            escape_table(&failure.source),
            escape_table(&failure.error)
        )?;
    }
    for failure in &comparison.failures {
        writeln!(
            output,
            "- Comparison input `{}`: {}",
            escape_table(&failure.source),
            escape_table(&failure.error)
        )?;
    }
    for issue in &pairing.issues {
        writeln!(output, "- {}", escape_table(issue))?;
    }
    if !baseline_revision.valid {
        writeln!(
            output,
            "- Baseline results do not contain one full revision SHA."
        )?;
    }
    if !comparison_revision.valid {
        writeln!(
            output,
            "- Comparison results do not contain one full revision SHA."
        )?;
    }
    if baseline_revision.valid
        && comparison_revision.valid
        && baseline_revision.label == comparison_revision.label
    {
        writeln!(output, "- Baseline and comparison revisions are identical.")?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_workload_identity(output: &mut String, pairs: &[RunPair<'_>]) -> Result<()> {
    writeln!(output, "## Workload identity\n")?;
    render_consumer_context(output)?;
    writeln!(
        output,
        "| Scenario | Profile | Total media consumers | Expected deliveries | Duration |"
    )?;
    writeln!(output, "| --- | --- | ---: | ---: | ---: |")?;
    for pair in pairs {
        let result = &pair.baseline.result;
        writeln!(
            output,
            "| {} | {} | {} | {} | {} s |",
            scenario_label(result.scenario),
            escape_table(&result.profile),
            grouped(result.plan.routes),
            grouped(result.plan.expected_deliveries),
            result.scenario.duration_seconds()
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_exact_delivery(output: &mut String, pairs: &[RunPair<'_>]) -> Result<()> {
    writeln!(output, "## Exact delivery correctness\n")?;
    writeln!(
        output,
        "| Scenario | Variant | Expected | Delivered | Missing | Duplicate | Out of order | Unexpected | Payload mismatch | Exact |"
    )?;
    writeln!(
        output,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |"
    )?;
    for pair in pairs {
        render_exact_row(output, "Baseline", pair.baseline)?;
        render_exact_row(output, "Comparison", pair.comparison)?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_exact_row(output: &mut String, role: &str, run: &RunData) -> Result<()> {
    let result = &run.result;
    let correctness = result.correctness;
    let validation = validate_run(run);
    writeln!(
        output,
        "| {} | {role} | {} | {} | {} | {} | {} | {} | {} | {} |",
        scenario_label(result.scenario),
        grouped(result.plan.expected_deliveries),
        grouped(result.delivered_packets),
        grouped(correctness.missing_packets),
        grouped(correctness.duplicate_packets),
        grouped(correctness.out_of_order_packets),
        grouped(correctness.unexpected_packets),
        grouped(correctness.payload_mismatches),
        validation.map_or_else(
            |error| escape_table(&format!("FAIL: {error:#}")),
            |()| "PASS".to_owned()
        )
    )?;
    Ok(())
}

fn render_graphs(
    output: &mut String,
    metrics: &[PairMetrics<'_>],
    baseline_revision: &Revision,
    comparison_revision: &Revision,
) -> Result<()> {
    writeln!(output, "## Comparison graphs\n")?;
    writeln!(
        output,
        "Every graph uses one scenario axis. The first line is baseline `{}`. The second line is comparison `{}`. Coincident lines mean both plotted values are equal and the tables retain both numbers.\n",
        short_revision(baseline_revision, "baseline"),
        short_revision(comparison_revision, "comparison")
    )?;
    render_delivery_graphs(output, metrics)?;
    render_system_graphs(output, metrics)
}

fn render_delivery_graphs(output: &mut String, metrics: &[PairMetrics<'_>]) -> Result<()> {
    render_paired_lines(
        output,
        metrics,
        "Receiver delivery throughput",
        "deliveries/s",
        0,
        |metric| {
            (
                Some(delivery_rate(&metric.pair.baseline.result)),
                Some(delivery_rate(&metric.pair.comparison.result)),
            )
        },
    )?;
    render_paired_lines(
        output,
        metrics,
        "Receiver-observed RTP payload",
        "RTP payload kbit/s",
        0,
        |metric| {
            (
                Some(delivered_payload_bits_per_second(&metric.pair.baseline.result) / 1_000),
                Some(delivered_payload_bits_per_second(&metric.pair.comparison.result) / 1_000),
            )
        },
    )?;
    Ok(())
}

fn render_system_graphs(output: &mut String, metrics: &[PairMetrics<'_>]) -> Result<()> {
    render_paired_lines(
        output,
        metrics,
        "SFU CPU time per million deliveries",
        "CPU ms / 1M deliveries",
        0,
        |metric| {
            (
                metric
                    .baseline
                    .server_cpu_micros_per_million_deliveries
                    .map(|value| value.div_ceil(1_000)),
                metric
                    .comparison
                    .server_cpu_micros_per_million_deliveries
                    .map(|value| value.div_ceil(1_000)),
            )
        },
    )?;
    render_paired_lines(
        output,
        metrics,
        "Generator send lag",
        "max send lag ms",
        0,
        |metric| {
            (
                Some(metric.pair.baseline.result.max_send_lag_ms),
                Some(metric.pair.comparison.result.max_send_lag_ms),
            )
        },
    )?;
    render_paired_lines(
        output,
        metrics,
        "SFU packet-loop delay",
        "max loop delay ms",
        0,
        |metric| {
            (
                metric.baseline.packet_loop_delay_ms,
                metric.comparison.packet_loop_delay_ms,
            )
        },
    )?;
    render_paired_lines(
        output,
        metrics,
        "SFU average CPU",
        "CPU (%)",
        100,
        |metric| {
            (
                metric.baseline.server_cpu_percent_milli.map(round_milli),
                metric.comparison.server_cpu_percent_milli.map(round_milli),
            )
        },
    )?;
    render_paired_lines(
        output,
        metrics,
        "SFU peak resident memory",
        "RSS MiB",
        0,
        |metric| {
            (
                metric.baseline.server_rss_bytes.map(bytes_to_mebibytes),
                metric.comparison.server_rss_bytes.map(bytes_to_mebibytes),
            )
        },
    )?;
    Ok(())
}

fn render_paired_lines<F>(
    output: &mut String,
    metrics: &[PairMetrics<'_>],
    title: &str,
    y_axis: &str,
    minimum: u64,
    metric_values: F,
) -> Result<()>
where
    F: Fn(&PairMetrics<'_>) -> (Option<u64>, Option<u64>),
{
    let mut paired = Vec::new();
    let mut omitted = Vec::new();
    for metric in metrics.iter().filter(|metric| metric.pair.workload_matches) {
        let label = chart_label(metric.pair.baseline.result.scenario);
        match metric_values(metric) {
            (Some(baseline_value), Some(comparison_value)) => {
                paired.push((label, baseline_value, comparison_value));
            }
            _ => omitted.push(label),
        }
    }
    if !omitted.is_empty() {
        writeln!(
            output,
            "The {title} graph omits scenarios without paired data: {}.\n",
            omitted.join(", ")
        )?;
    }
    if paired.is_empty() {
        writeln!(output, "The {title} graph has no paired data.\n")?;
        return Ok(());
    }
    let labels = paired
        .iter()
        .map(|(label, _baseline, _comparison)| label.clone())
        .collect::<Vec<_>>();
    let baseline = paired
        .iter()
        .map(|(_label, baseline, _comparison)| *baseline)
        .collect::<Vec<_>>();
    let comparison = paired
        .iter()
        .map(|(_label, _baseline, comparison)| *comparison)
        .collect::<Vec<_>>();
    render_category_charts(
        output,
        title,
        "Two lines compare both revisions on one scenario axis.",
        &labels,
        y_axis,
        minimum,
        &[
            ChartSeries {
                name: "baseline",
                values: &baseline,
            },
            ChartSeries {
                name: "comparison",
                values: &comparison,
            },
        ],
    )
}

fn render_performance_table(output: &mut String, metrics: &[PairMetrics<'_>]) -> Result<()> {
    writeln!(output, "## Performance deltas\n")?;
    writeln!(
        output,
        "| Scenario | Metric | Baseline | Comparison | Delta |"
    )?;
    writeln!(output, "| --- | --- | ---: | ---: | ---: |")?;
    for metric in metrics.iter().filter(|metric| metric.pair.workload_matches) {
        let label = scenario_label(metric.pair.baseline.result.scenario);
        render_metric_row(
            output,
            &label,
            "SFU CPU time per million deliveries",
            metric.baseline.server_cpu_micros_per_million_deliveries,
            metric.comparison.server_cpu_micros_per_million_deliveries,
            format_cpu_time,
        )?;
        render_metric_row(
            output,
            &label,
            "Receiver deliveries",
            Some(delivery_rate(&metric.pair.baseline.result)),
            Some(delivery_rate(&metric.pair.comparison.result)),
            format_deliveries_per_second,
        )?;
        render_metric_row(
            output,
            &label,
            "Receiver-observed RTP payload",
            Some(delivered_payload_bits_per_second(
                &metric.pair.baseline.result,
            )),
            Some(delivered_payload_bits_per_second(
                &metric.pair.comparison.result,
            )),
            format_bits_per_second,
        )?;
        render_metric_row(
            output,
            &label,
            "Generator max send lag",
            Some(metric.pair.baseline.result.max_send_lag_ms),
            Some(metric.pair.comparison.result.max_send_lag_ms),
            format_milliseconds,
        )?;
        render_metric_row(
            output,
            &label,
            "Packet-loop max delay",
            metric.baseline.packet_loop_delay_ms,
            metric.comparison.packet_loop_delay_ms,
            format_milliseconds,
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_resource_table(output: &mut String, metrics: &[PairMetrics<'_>]) -> Result<()> {
    writeln!(output, "## Resource deltas\n")?;
    writeln!(
        output,
        "CPU averages are weighted by sample interval. RSS and CPU peaks are sampled maxima.\n"
    )?;
    writeln!(
        output,
        "| Scenario | Metric | Baseline | Comparison | Delta |"
    )?;
    writeln!(output, "| --- | --- | ---: | ---: | ---: |")?;
    for metric in metrics.iter().filter(|metric| metric.pair.workload_matches) {
        let label = scenario_label(metric.pair.baseline.result.scenario);
        render_metric_row(
            output,
            &label,
            "SFU CPU average",
            metric.baseline.server_cpu_percent_milli,
            metric.comparison.server_cpu_percent_milli,
            format_cpu_percent,
        )?;
        render_metric_row(
            output,
            &label,
            "SFU CPU peak",
            metric.baseline.server_cpu_peak_percent_milli,
            metric.comparison.server_cpu_peak_percent_milli,
            format_cpu_percent,
        )?;
        render_metric_row(
            output,
            &label,
            "SFU RSS peak",
            metric.baseline.server_rss_bytes,
            metric.comparison.server_rss_bytes,
            format_mebibytes,
        )?;
        render_metric_row(
            output,
            &label,
            "RTC CPU average",
            metric.baseline.rtc_cpu_percent_milli,
            metric.comparison.rtc_cpu_percent_milli,
            format_cpu_percent,
        )?;
        render_metric_row(
            output,
            &label,
            "RTC RSS peak",
            metric.baseline.rtc_rss_bytes,
            metric.comparison.rtc_rss_bytes,
            format_mebibytes,
        )?;
        render_metric_row(
            output,
            &label,
            "SFU forwarded packets",
            metric.baseline.forwarded_packets_per_second,
            metric.comparison.forwarded_packets_per_second,
            format_packets_per_second,
        )?;
        render_metric_row(
            output,
            &label,
            "SFU sampled egress payload",
            metric.baseline.egress_payload_bits_per_second,
            metric.comparison.egress_payload_bits_per_second,
            format_bits_per_second,
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_metric_row(
    output: &mut String,
    scenario: &str,
    metric: &str,
    baseline: Option<u64>,
    comparison: Option<u64>,
    format: fn(u64) -> String,
) -> Result<()> {
    writeln!(
        output,
        "| {scenario} | {metric} | {} | {} | {} |",
        format_optional(baseline, format),
        format_optional(comparison, format),
        format_delta(baseline, comparison, format)
    )?;
    Ok(())
}

fn pair_runs<'a>(baseline: &'a [RunData], comparison: &'a [RunData]) -> Pairing<'a> {
    let mut specs = baseline
        .iter()
        .chain(comparison)
        .map(|run| run.result.scenario)
        .collect::<Vec<_>>();
    specs.sort_unstable_by_key(|spec| scenario_key(*spec));
    specs.dedup();
    let mut pairs = Vec::new();
    let mut issues = Vec::new();
    for spec in specs {
        let baseline_runs = baseline
            .iter()
            .filter(|run| run.result.scenario == spec)
            .collect::<Vec<_>>();
        let comparison_runs = comparison
            .iter()
            .filter(|run| run.result.scenario == spec)
            .collect::<Vec<_>>();
        match (baseline_runs.as_slice(), comparison_runs.as_slice()) {
            ([baseline_run], [comparison_run]) => {
                let workload_matches = workload_matches(baseline_run, comparison_run);
                if !workload_matches {
                    issues.push(format!(
                        "{} has different workload contracts",
                        scenario_label(spec)
                    ));
                }
                pairs.push(RunPair {
                    baseline: baseline_run,
                    comparison: comparison_run,
                    workload_matches,
                });
            }
            ([], _) => issues.push(format!(
                "{} is missing from the baseline",
                scenario_label(spec)
            )),
            (_, []) => issues.push(format!(
                "{} is missing from the comparison",
                scenario_label(spec)
            )),
            _ => issues.push(format!(
                "{} appears more than once on one side",
                scenario_label(spec)
            )),
        }
    }
    Pairing { pairs, issues }
}

fn workload_matches(baseline: &RunData, comparison: &RunData) -> bool {
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

fn telemetry(run: &RunData) -> TelemetrySummary {
    TelemetrySummary::from_samples(run.samples.as_ref(), run.result.delivered_packets)
}

fn pair_performance_valid(metric: &PairMetrics<'_>) -> bool {
    metric.pair.workload_matches
        && pacing_valid(&metric.pair.baseline.result)
        && pacing_valid(&metric.pair.comparison.result)
        && telemetry_complete(&metric.baseline)
        && telemetry_complete(&metric.comparison)
}

fn telemetry_complete(summary: &TelemetrySummary) -> bool {
    summary.samples > 0
        && summary.unavailable == 0
        && summary.server_cpu_micros_per_million_deliveries.is_some()
        && summary.server_cpu_percent_milli.is_some()
        && summary.server_cpu_peak_percent_milli.is_some()
        && summary.server_rss_bytes.is_some()
        && summary.rtc_cpu_percent_milli.is_some()
        && summary.rtc_rss_bytes.is_some()
        && summary.packet_loop_delay_ms.is_some()
        && summary.forwarded_packets_per_second.is_some()
        && summary.egress_payload_bits_per_second.is_some()
}

fn revision(runs: &[RunData]) -> Revision {
    let revisions = runs
        .iter()
        .filter_map(|run| run.result.o_sfu_revision.as_deref())
        .collect::<BTreeSet<_>>();
    let missing = runs.iter().any(|run| run.result.o_sfu_revision.is_none());
    match (revisions.len(), missing, revisions.first()) {
        (1, false, Some(revision)) => Revision {
            label: (*revision).to_owned(),
            valid: valid_revision(revision),
        },
        (0, _, _) => Revision {
            label: "n/a".to_owned(),
            valid: false,
        },
        _ => Revision {
            label: "mixed".to_owned(),
            valid: false,
        },
    }
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn short_revision<'a>(revision: &'a Revision, fallback: &'a str) -> &'a str {
    if revision.valid {
        revision.label.get(..12).unwrap_or(fallback)
    } else {
        fallback
    }
}

fn format_optional(value: Option<u64>, format: fn(u64) -> String) -> String {
    value.map_or_else(|| "n/a".to_owned(), format)
}

fn format_delta(
    baseline: Option<u64>,
    comparison: Option<u64>,
    format: fn(u64) -> String,
) -> String {
    let (Some(baseline), Some(comparison)) = (baseline, comparison) else {
        return "n/a".to_owned();
    };
    let (sign, magnitude) = match comparison.cmp(&baseline) {
        Ordering::Greater => ("+", comparison - baseline),
        Ordering::Less => ("-", baseline - comparison),
        Ordering::Equal => ("", 0),
    };
    let absolute = format(magnitude);
    percentage_delta(baseline, magnitude).map_or_else(
        || format!("{sign}{absolute} (n/a)"),
        |tenths| {
            let percent_sign = if magnitude == 0 { "" } else { sign };
            format!(
                "{sign}{absolute} ({percent_sign}{}.{:01}%)",
                tenths / 10,
                tenths % 10
            )
        },
    )
}

fn percentage_delta(baseline: u64, magnitude: u64) -> Option<u64> {
    if baseline == 0 {
        return None;
    }
    let tenths = u128::from(magnitude) * 1_000 / u128::from(baseline);
    Some(u64::try_from(tenths).unwrap_or(u64::MAX))
}

fn format_cpu_time(micros: u64) -> String {
    format!(
        "{}.{:06} CPU s/1M",
        grouped(micros / 1_000_000),
        micros % 1_000_000
    )
}

fn format_deliveries_per_second(value: u64) -> String {
    format!("{} deliveries/s", grouped(value))
}

fn format_packets_per_second(value: u64) -> String {
    format!("{} packets/s", grouped(value))
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

fn round_milli(value: u64) -> u64 {
    value.saturating_add(500) / 1_000
}

fn bytes_to_mebibytes(value: u64) -> u64 {
    value.div_ceil(1024 * 1024)
}

const fn pass_fail(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}

#[cfg(test)]
#[path = "TESTS/comparison_tests.rs"]
mod tests;
