use std::{
    fmt::{self, Write as _},
    fs,
    iter::once,
    mem::take,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use plotters::{
    chart::{ChartContext, SeriesLabelPosition},
    coord::{Shift, cartesian::Cartesian2d, types::RangedCoordu64},
    prelude::{
        BLACK, ChartBuilder, Circle, DrawingArea, DrawingBackend, IntoDrawingArea, IntoFont,
        LineSeries, PathElement, RGBColor, SVGBackend, Text, WHITE,
    },
    style::{
        Color as _, ShapeStyle,
        text_anchor::{HPos, Pos, VPos},
    },
};

use super::{
    MAX_TELEMETRY_WORKERS, PhaseMarker, RunData, SampleSet, TelemetrySample, escape_table,
    load_run, scenario_key, scenario_label, valid_repository_component, workload_matches,
};

const DASHBOARD_SIZE: (u32, u32) = (1_800, 2_600);
const SMOOTHING_RADIUS: usize = 2;
const COLOR_BLUE: RGBColor = RGBColor(56, 139, 253);
const COLOR_ORANGE: RGBColor = RGBColor(219, 109, 40);
const COLOR_GREEN: RGBColor = RGBColor(35, 134, 54);
const COLOR_PURPLE: RGBColor = RGBColor(130, 80, 223);
const COLOR_RED: RGBColor = RGBColor(207, 34, 46);
const COLOR_VIOLET: RGBColor = RGBColor(111, 66, 193);

#[derive(Clone, Copy)]
pub struct DashboardConfig<'a> {
    pub output_directory: &'a Path,
    pub asset_stem: &'a str,
    pub public_url_base: Option<&'a str>,
}

#[derive(Clone, Copy)]
enum RevisionRole {
    Single,
    Baseline,
    Comparison,
}

struct DashboardAsset {
    scenario: String,
    role: RevisionRole,
    revision: String,
    file_name: String,
}

struct DashboardData {
    elapsed_ms: u64,
    phases: Vec<PhaseMarker>,
    panels: Vec<Panel>,
}

struct Panel {
    title: &'static str,
    unit: Unit,
    series: Vec<Series>,
}

struct Series {
    name: String,
    color: RGBColor,
    raw_values: Option<Vec<Option<(u64, u64)>>>,
    values: Vec<Option<(u64, u64)>>,
    interval_starts_ms: Option<Vec<Option<u64>>>,
    points_only: bool,
    smooth: bool,
}

#[derive(Clone, Copy)]
enum Unit {
    Cpu,
    Mebibytes,
    PacketsPerSecond,
    MegabitsPerSecond,
    Multiplier,
    Milliseconds,
    Count,
    Percent,
}

#[derive(Clone)]
struct DashboardLimits {
    elapsed_ms: u64,
    panel_maxima: Vec<u64>,
}

impl DashboardData {
    fn from_run(run: &RunData, samples: &SampleSet) -> Result<Self> {
        Self::new(
            samples,
            expected_fanout_milli(run),
            run.result.server_policy.media_workers,
        )
    }

    fn new(
        samples: &SampleSet,
        expected_fanout_milli: Option<u64>,
        media_workers: u32,
    ) -> Result<Self> {
        let elapsed_ms = samples
            .samples
            .iter()
            .map(|sample| sample.elapsed_ms)
            .chain(samples.phases.iter().map(|marker| marker.elapsed_ms))
            .max()
            .unwrap_or(1)
            .max(1);
        let mut panels = vec![
            cpu_panel(&samples.samples),
            memory_panel(&samples.samples),
            packet_rate_panel(&samples.samples),
            payload_rate_panel(&samples.samples),
            fanout_panel(&samples.samples, expected_fanout_milli),
            worker_bitrate_panel(&samples.samples, media_workers)?,
            packet_loop_panel(&samples.samples),
            queue_panel(&samples.samples),
            pressure_panel(&samples.samples),
            scrape_panel(&samples.samples),
        ];
        for panel in &mut panels {
            for series in &mut panel.series {
                gap_phase_crossings(series, &samples.phases);
            }
        }
        Ok(Self {
            elapsed_ms,
            phases: samples.phases.clone(),
            panels,
        })
    }

    fn limits(&self) -> DashboardLimits {
        DashboardLimits {
            elapsed_ms: self.elapsed_ms.max(1),
            panel_maxima: self
                .panels
                .iter()
                .map(|panel| padded_maximum(panel.maximum()))
                .collect(),
        }
    }
}

fn cpu_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Process CPU",
        unit: Unit::Cpu,
        series: vec![
            direct_series(
                "SFU",
                COLOR_BLUE,
                samples,
                |sample| sample.server_cpu_percent_milli,
                true,
            ),
            direct_series(
                "RTC generator",
                COLOR_ORANGE,
                samples,
                |sample| sample.rtc_cpu_percent_milli,
                true,
            ),
        ],
    }
}

fn memory_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Resident memory",
        unit: Unit::Mebibytes,
        series: vec![
            direct_series(
                "SFU",
                COLOR_BLUE,
                samples,
                |sample| sample.server_rss_bytes,
                false,
            ),
            direct_series(
                "RTC generator",
                COLOR_ORANGE,
                samples,
                |sample| sample.rtc_rss_bytes,
                false,
            ),
        ],
    }
}

fn packet_rate_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "RTP packets / second",
        unit: Unit::PacketsPerSecond,
        series: vec![
            counter_series(
                "Ingress",
                COLOR_BLUE,
                samples,
                |sample| sample.ingress_packets,
                1_000,
            ),
            counter_series(
                "Forwarded local",
                COLOR_GREEN,
                samples,
                |sample| sample.forwarded_packets,
                1_000,
            ),
            counter_series(
                "Egress",
                COLOR_ORANGE,
                samples,
                |sample| sample.egress_packets,
                1_000,
            ),
        ],
    }
}

fn payload_rate_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "RTP payload Mbit / second",
        unit: Unit::MegabitsPerSecond,
        series: vec![
            counter_series(
                "Ingress",
                COLOR_BLUE,
                samples,
                |sample| sample.ingress_payload_bytes,
                8_000,
            ),
            counter_series(
                "Forwarded local",
                COLOR_GREEN,
                samples,
                |sample| sample.forwarded_payload_bytes,
                8_000,
            ),
            counter_series(
                "Egress",
                COLOR_ORANGE,
                samples,
                |sample| sample.egress_payload_bytes,
                8_000,
            ),
        ],
    }
}

fn fanout_panel(samples: &[TelemetrySample], expected_fanout_milli: Option<u64>) -> Panel {
    let mut series = vec![fanout_series(samples)];
    if let Some(expected) = expected_fanout_milli {
        series.push(Series {
            name: "Expected".to_owned(),
            color: COLOR_PURPLE,
            raw_values: None,
            values: samples
                .iter()
                .map(|sample| (!sample.is_gap).then_some((sample.elapsed_ms, expected)))
                .collect(),
            interval_starts_ms: None,
            points_only: false,
            smooth: false,
        });
    }
    Panel {
        title: "Local fanout multiplier",
        unit: Unit::Multiplier,
        series,
    }
}

fn worker_bitrate_panel(samples: &[TelemetrySample], media_workers: u32) -> Result<Panel> {
    Ok(Panel {
        title: "Worker egress Mbit / second",
        unit: Unit::MegabitsPerSecond,
        series: worker_series(samples, media_workers)?,
    })
}

fn packet_loop_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Packet-loop delay",
        unit: Unit::Milliseconds,
        series: vec![direct_series(
            "Maximum worker",
            COLOR_RED,
            samples,
            |sample| sample.packet_loop_delay_ms,
            false,
        )],
    }
}

fn queue_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Queue depth",
        unit: Unit::Count,
        series: vec![
            direct_series(
                "Maximum command backlog",
                COLOR_PURPLE,
                samples,
                max_command_backlog,
                false,
            ),
            direct_series(
                "Maximum relay mailbox",
                COLOR_RED,
                samples,
                max_relay_backlog,
                false,
            ),
        ],
    }
}

fn pressure_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Worker pressure score",
        unit: Unit::Percent,
        series: vec![direct_series(
            "Maximum worker",
            COLOR_PURPLE,
            samples,
            max_worker_pressure,
            false,
        )],
    }
}

fn scrape_panel(samples: &[TelemetrySample]) -> Panel {
    Panel {
        title: "Telemetry scrape duration",
        unit: Unit::Milliseconds,
        series: vec![
            direct_series(
                "Successful scrape",
                COLOR_VIOLET,
                samples,
                successful_scrape_duration,
                false,
            ),
            point_series(
                "Unavailable scrape",
                COLOR_RED,
                samples,
                unavailable_scrape_duration,
            ),
        ],
    }
}

fn expected_fanout_milli(run: &RunData) -> Option<u64> {
    let offered = run.result.plan.offered_packets;
    if offered == 0 {
        return None;
    }
    let scaled = u128::from(run.result.plan.expected_deliveries) * 1_000;
    Some(u64::try_from(scaled / u128::from(offered)).unwrap_or(u64::MAX))
}

fn gap_phase_crossings(series: &mut Series, phases: &[PhaseMarker]) {
    let Some(starts) = series.interval_starts_ms.as_ref() else {
        return;
    };
    let crossings = series
        .values
        .iter()
        .zip(starts)
        .enumerate()
        .filter_map(|(index, (value, start_ms))| {
            let (Some((elapsed_ms, _value)), Some(start_ms)) = (*value, *start_ms) else {
                return None;
            };
            phases
                .iter()
                .any(|marker| start_ms < marker.elapsed_ms && marker.elapsed_ms <= elapsed_ms)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if crossings.is_empty() {
        return;
    }
    series
        .raw_values
        .get_or_insert_with(|| series.values.clone());
    for index in crossings {
        if let Some(value) = series.values.get_mut(index) {
            *value = None;
        }
    }
}

pub(super) fn write_single(
    inputs: &[PathBuf],
    config: &DashboardConfig<'_>,
    artifact_url: Option<&str>,
) -> Result<String> {
    validate_config(config)?;
    prepare_output_directory(config.output_directory)?;
    let mut runs = inputs
        .iter()
        .filter_map(|input| load_run(input).ok())
        .collect::<Vec<_>>();
    runs.sort_unstable_by_key(|run| scenario_key(run.result.scenario));
    let mut assets = Vec::new();
    let mut scenarios = Vec::new();
    for candidate in &runs {
        let scenario = candidate.result.scenario;
        if scenarios.contains(&scenario) {
            continue;
        }
        scenarios.push(scenario);
        let Some(run) = unique_scenario(&runs, scenario) else {
            continue;
        };
        let Some(samples) = run
            .samples
            .as_ref()
            .filter(|samples| !samples.samples.is_empty())
        else {
            continue;
        };
        assets.push(write_asset(run, samples, RevisionRole::Single, config)?);
    }
    render_assets(&assets, config.public_url_base, artifact_url)
}

pub(crate) fn write_comparison(
    baseline_inputs: &[PathBuf],
    comparison_inputs: &[PathBuf],
    config: &DashboardConfig<'_>,
    artifact_url: Option<&str>,
) -> Result<String> {
    validate_config(config)?;
    prepare_output_directory(config.output_directory)?;
    let baseline = load_runs(baseline_inputs);
    let comparison = load_runs(comparison_inputs);
    let mut assets = Vec::new();
    let mut scenarios = Vec::new();
    for baseline_run in &baseline {
        let scenario = baseline_run.result.scenario;
        if scenarios.contains(&scenario) {
            continue;
        }
        scenarios.push(scenario);
        let Some((baseline_run, comparison_run)) = matching_pair(&baseline, &comparison, scenario)
        else {
            continue;
        };
        let (Some(baseline_samples), Some(comparison_samples)) = (
            baseline_run
                .samples
                .as_ref()
                .filter(|samples| !samples.samples.is_empty()),
            comparison_run
                .samples
                .as_ref()
                .filter(|samples| !samples.samples.is_empty()),
        ) else {
            continue;
        };
        let baseline_data = DashboardData::from_run(baseline_run, baseline_samples)?;
        let comparison_data = DashboardData::from_run(comparison_run, comparison_samples)?;
        let limits = DashboardLimits::shared(&baseline_data, &comparison_data);
        assets.push(write_asset_data(
            baseline_run,
            &baseline_data,
            RevisionRole::Baseline,
            config,
            &limits,
        )?);
        assets.push(write_asset_data(
            comparison_run,
            &comparison_data,
            RevisionRole::Comparison,
            config,
            &limits,
        )?);
    }
    render_assets(&assets, config.public_url_base, artifact_url)
}

fn load_runs(inputs: &[PathBuf]) -> Vec<RunData> {
    let mut runs = inputs
        .iter()
        .filter_map(|input| load_run(input).ok())
        .collect::<Vec<_>>();
    runs.sort_unstable_by_key(|run| scenario_key(run.result.scenario));
    runs
}

fn unique_scenario(runs: &[RunData], scenario: crate::ScenarioSpec) -> Option<&RunData> {
    let mut matching = runs.iter().filter(|run| run.result.scenario == scenario);
    let run = matching.next()?;
    matching.next().is_none().then_some(run)
}

fn matching_pair<'a>(
    baseline: &'a [RunData],
    comparison: &'a [RunData],
    scenario: crate::ScenarioSpec,
) -> Option<(&'a RunData, &'a RunData)> {
    let baseline = unique_scenario(baseline, scenario)?;
    let comparison = unique_scenario(comparison, scenario)?;
    workload_matches(baseline, comparison).then_some((baseline, comparison))
}

fn write_asset(
    run: &RunData,
    samples: &SampleSet,
    role: RevisionRole,
    config: &DashboardConfig<'_>,
) -> Result<DashboardAsset> {
    let data = DashboardData::from_run(run, samples)?;
    let scenario = scenario_label(run.result.scenario);
    ensure!(
        valid_scenario_id(&scenario),
        "invalid dashboard scenario ID"
    );
    let file_name = format!("{}-{role}-{scenario}.svg", config.asset_stem);
    let output = config.output_directory.join(&file_name);
    let revision = run
        .result
        .o_sfu_revision
        .clone()
        .unwrap_or_else(|| "revision unavailable".to_owned());
    render_svg(&data, &format!("{scenario} | {role} | {revision}"), &output)?;
    Ok(DashboardAsset {
        scenario,
        role,
        revision,
        file_name,
    })
}

fn write_asset_data(
    run: &RunData,
    data: &DashboardData,
    role: RevisionRole,
    config: &DashboardConfig<'_>,
    limits: &DashboardLimits,
) -> Result<DashboardAsset> {
    let scenario = scenario_label(run.result.scenario);
    ensure!(
        valid_scenario_id(&scenario),
        "invalid dashboard scenario ID"
    );
    let file_name = format!("{}-{role}-{scenario}.svg", config.asset_stem);
    let output = config.output_directory.join(&file_name);
    let revision = run
        .result
        .o_sfu_revision
        .clone()
        .unwrap_or_else(|| "revision unavailable".to_owned());
    let title = format!("{scenario} | {role} | {revision}");
    render_svg_with_limits(data, &title, &output, limits)?;
    Ok(DashboardAsset {
        scenario,
        role,
        revision,
        file_name,
    })
}

fn prepare_output_directory(output: &Path) -> Result<()> {
    fs::create_dir_all(output)
        .with_context(|| format!("failed to create dashboard directory {}", output.display()))
}

fn validate_config(config: &DashboardConfig<'_>) -> Result<()> {
    ensure!(
        !config.asset_stem.is_empty()
            && config
                .asset_stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "dashboard asset stem must contain only ASCII letters, digits and hyphens"
    );
    if let Some(base) = config.public_url_base {
        validate_public_url_base(base)?;
    }
    Ok(())
}

fn validate_public_url_base(base: &str) -> Result<()> {
    let path = base
        .strip_prefix("https://github.com/")
        .context("dashboard URL base must use https://github.com")?;
    let mut parts = path.split('/');
    let (Some(owner), Some(repository), Some(releases), Some(download), Some(tag), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        anyhow::bail!("dashboard URL base must identify the load-test-assets release");
    };
    ensure!(
        valid_repository_component(owner)
            && valid_repository_component(repository)
            && releases == "releases"
            && download == "download"
            && tag == "load-test-assets",
        "dashboard URL base must identify the load-test-assets release"
    );
    Ok(())
}

fn valid_scenario_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn render_assets(
    assets: &[DashboardAsset],
    public_url_base: Option<&str>,
    artifact_url: Option<&str>,
) -> Result<String> {
    if assets.is_empty() {
        return Ok(String::new());
    }
    let mut output = String::new();
    writeln!(output, "## Telemetry dashboards\n")?;
    writeln!(
        output,
        "Each dashboard uses the actual scrape timestamps. CPU, traffic and worker-bitrate panels retain faint raw points behind a centered moving average over up to five contiguous samples. Smoothed lines exclude phase-crossing intervals while the individual mixed-phase observations remain visible only as points. Missing values and counter resets remain gaps. Packet and payload rates use adjacent counter deltas. Observed fanout uses adjacent forwarded-local and ingress packet deltas. Expected fanout is the whole-workload ratio of planned deliveries to offered packets. Delay, queue and pressure lines are per-sample worker maxima. Unavailable scrapes are red points.\n"
    )?;
    let Some(public_url_base) = public_url_base else {
        if artifact_url.is_some() {
            writeln!(
                output,
                "The workflow artifact contains {} Plotters SVG dashboard{} and the source `samples.jsonl` files.\n",
                assets.len(),
                if assets.len() == 1 { "" } else { "s" }
            )?;
        } else {
            writeln!(
                output,
                "Generated {} Plotters SVG dashboard{} in the configured output directory.\n",
                assets.len(),
                if assets.len() == 1 { "" } else { "s" }
            )?;
        }
        return Ok(output);
    };
    let mut scenarios = Vec::new();
    for asset in assets {
        if !scenarios.contains(&asset.scenario.as_str()) {
            scenarios.push(asset.scenario.as_str());
        }
    }
    let open_scenario = assets.last().map(|asset| asset.scenario.as_str());
    for scenario in scenarios {
        let open = if Some(scenario) == open_scenario {
            " open"
        } else {
            ""
        };
        writeln!(output, "<details{open}>")?;
        writeln!(output, "<summary>{scenario}</summary>\n")?;
        for asset in assets.iter().filter(|asset| asset.scenario == scenario) {
            let png = asset.file_name.trim_end_matches(".svg").to_owned() + ".png";
            let image_url = format!("{public_url_base}/{png}");
            let target = artifact_url.unwrap_or(&image_url);
            writeln!(
                output,
                "**{}** `{}`\n\n[![{} telemetry dashboard]({image_url})]({target})\n",
                asset.role,
                escape_table(&asset.revision),
                asset.role
            )?;
        }
        writeln!(output, "</details>\n")?;
    }
    Ok(output)
}

impl fmt::Display for RevisionRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Single => "single",
            Self::Baseline => "baseline",
            Self::Comparison => "comparison",
        })
    }
}

const fn worker_color(index: usize) -> RGBColor {
    match index % 6 {
        0 => COLOR_BLUE,
        1 => COLOR_ORANGE,
        2 => COLOR_GREEN,
        3 => COLOR_PURPLE,
        4 => COLOR_RED,
        _ => COLOR_VIOLET,
    }
}

impl Panel {
    fn maximum(&self) -> u64 {
        self.series
            .iter()
            .flat_map(|series| {
                series
                    .values
                    .iter()
                    .chain(series.raw_values.as_deref().unwrap_or_default().iter())
            })
            .filter_map(|value| value.map(|(_elapsed_ms, value)| value))
            .max()
            .unwrap_or_default()
    }
}

impl DashboardLimits {
    fn shared(left: &DashboardData, right: &DashboardData) -> Self {
        let mut limits = left.limits();
        let right = right.limits();
        limits.elapsed_ms = limits.elapsed_ms.max(right.elapsed_ms);
        for (left, right) in limits.panel_maxima.iter_mut().zip(right.panel_maxima) {
            *left = (*left).max(right);
        }
        limits
    }
}

fn direct_series(
    name: impl Into<String>,
    color: RGBColor,
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
    smooth: bool,
) -> Series {
    let values = samples
        .iter()
        .map(|sample| value(sample).map(|value| (sample.elapsed_ms, value)))
        .collect::<Vec<_>>();
    Series {
        name: name.into(),
        color,
        raw_values: None,
        interval_starts_ms: smooth.then(|| previous_value_starts(&values)),
        values,
        points_only: false,
        smooth,
    }
}

fn point_series(
    name: impl Into<String>,
    color: RGBColor,
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
) -> Series {
    let mut series = direct_series(name, color, samples, value, false);
    series.points_only = true;
    series
}

fn counter_series(
    name: impl Into<String>,
    color: RGBColor,
    samples: &[TelemetrySample],
    value: fn(&TelemetrySample) -> Option<u64>,
    scale: u64,
) -> Series {
    let mut previous = None;
    let mut interval_starts_ms = Vec::with_capacity(samples.len());
    let values = samples
        .iter()
        .map(|sample| {
            let current = value(sample).map(|value| (sample.elapsed_ms, value));
            let prior = previous;
            previous = current;
            if let (Some((previous_ms, previous_value)), Some((elapsed_ms, current_value))) =
                (prior, current)
            {
                let point = elapsed_ms
                    .checked_sub(previous_ms)
                    .filter(|elapsed| *elapsed > 0)
                    .and_then(|elapsed| {
                        current_value.checked_sub(previous_value).map(|delta| {
                            let rate = u128::from(delta) * u128::from(scale) / u128::from(elapsed);
                            (elapsed_ms, u64::try_from(rate).unwrap_or(u64::MAX))
                        })
                    });
                interval_starts_ms.push(point.map(|_point| previous_ms));
                point
            } else {
                interval_starts_ms.push(None);
                None
            }
        })
        .collect();
    Series {
        name: name.into(),
        color,
        raw_values: None,
        values,
        interval_starts_ms: Some(interval_starts_ms),
        points_only: false,
        smooth: true,
    }
}

fn fanout_series(samples: &[TelemetrySample]) -> Series {
    let mut previous = None;
    let mut interval_starts_ms = Vec::with_capacity(samples.len());
    let values = samples
        .iter()
        .map(|sample| {
            let current = Some((
                sample.elapsed_ms,
                sample.ingress_packets?,
                sample.forwarded_packets?,
            ));
            let prior = previous;
            previous = current;
            if let (
                Some((previous_ms, previous_ingress, previous_forwarded)),
                Some((elapsed_ms, ingress, forwarded)),
            ) = (prior, current)
            {
                let point = elapsed_ms
                    .checked_sub(previous_ms)
                    .filter(|elapsed| *elapsed > 0)
                    .and_then(|_elapsed| {
                        ingress
                            .checked_sub(previous_ingress)
                            .zip(forwarded.checked_sub(previous_forwarded))
                    })
                    .and_then(|(ingress, forwarded)| {
                        (ingress > 0).then(|| {
                            let multiplier = u128::from(forwarded) * 1_000 / u128::from(ingress);
                            (elapsed_ms, u64::try_from(multiplier).unwrap_or(u64::MAX))
                        })
                    });
                interval_starts_ms.push(point.map(|_point| previous_ms));
                point
            } else {
                interval_starts_ms.push(None);
                None
            }
        })
        .collect();
    Series {
        name: "Observed".to_owned(),
        color: COLOR_GREEN,
        raw_values: None,
        values,
        interval_starts_ms: Some(interval_starts_ms),
        points_only: false,
        smooth: true,
    }
}

fn worker_series(samples: &[TelemetrySample], media_workers: u32) -> Result<Vec<Series>> {
    type WorkerValues = Vec<Option<(u64, u64)>>;

    let media_workers =
        usize::try_from(media_workers).context("media-worker policy exceeds platform capacity")?;
    ensure!(media_workers > 0, "result policy has no media workers");
    ensure!(
        media_workers <= MAX_TELEMETRY_WORKERS,
        "dashboard supports at most {MAX_TELEMETRY_WORKERS} media workers"
    );
    let mut workers: Vec<Option<WorkerValues>> = vec![None; media_workers];
    for (sample_index, sample) in samples.iter().enumerate() {
        ensure!(
            sample.workers.len() <= media_workers,
            "telemetry sample at {} ms contains {} workers but result policy allows {} media workers",
            sample.elapsed_ms,
            sample.workers.len(),
            media_workers
        );
        for values in workers.iter_mut().flatten() {
            values.push(None);
        }
        for worker in &sample.workers {
            let values = workers.get_mut(worker.media_worker_id).with_context(|| {
                format!(
                    "telemetry sample at {} ms contains worker {} outside result policy of {} media workers",
                    sample.elapsed_ms, worker.media_worker_id, media_workers
                )
            })?;
            let values = values.get_or_insert_with(|| vec![None; sample_index.saturating_add(1)]);
            let current = values
                .last_mut()
                .context("worker series has no current sample")?;
            ensure!(
                current.is_none(),
                "telemetry sample at {} ms repeats worker {}",
                sample.elapsed_ms,
                worker.media_worker_id
            );
            *current = Some((sample.elapsed_ms, worker.egress_bitrate_bps));
        }
    }
    Ok(workers
        .into_iter()
        .enumerate()
        .filter_map(|(worker_id, values)| {
            values.map(|values| Series {
                name: format!("Worker {worker_id}"),
                color: worker_color(worker_id),
                raw_values: None,
                interval_starts_ms: Some(previous_value_starts(&values)),
                values,
                points_only: false,
                smooth: true,
            })
        })
        .collect())
}

fn previous_value_starts(values: &[Option<(u64, u64)>]) -> Vec<Option<u64>> {
    let mut previous = None;
    values
        .iter()
        .map(|value| match *value {
            Some((elapsed_ms, _value)) => {
                let start = previous;
                previous = Some(elapsed_ms);
                start
            }
            None => None,
        })
        .collect()
}

fn successful_scrape_duration(sample: &TelemetrySample) -> Option<u64> {
    (!sample.is_gap)
        .then_some(sample.scrape_duration_ms)
        .flatten()
}

fn unavailable_scrape_duration(sample: &TelemetrySample) -> Option<u64> {
    sample.is_gap.then_some(sample.scrape_duration_ms).flatten()
}

fn max_command_backlog(sample: &TelemetrySample) -> Option<u64> {
    sample
        .workers
        .iter()
        .map(|worker| u64::try_from(worker.command_backlog_depth).unwrap_or(u64::MAX))
        .max()
}

fn max_relay_backlog(sample: &TelemetrySample) -> Option<u64> {
    sample
        .workers
        .iter()
        .map(|worker| u64::try_from(worker.relay_mailbox_depth).unwrap_or(u64::MAX))
        .max()
}

fn max_worker_pressure(sample: &TelemetrySample) -> Option<u64> {
    sample
        .workers
        .iter()
        .map(|worker| u64::from(worker.worker_pressure_score))
        .max()
}

fn render_svg(data: &DashboardData, title: &str, output: &Path) -> Result<()> {
    render_svg_with_limits(data, title, output, &data.limits())
}

fn render_svg_with_limits(
    data: &DashboardData,
    title: &str,
    output: &Path,
    limits: &DashboardLimits,
) -> Result<()> {
    let root = SVGBackend::new(output, DASHBOARD_SIZE).into_drawing_area();
    root.fill(&WHITE)
        .context("failed to fill dashboard background")?;
    let root = root
        .titled(title, ("sans-serif", 36).into_font().color(&BLACK))
        .context("failed to render dashboard title")?;
    let panels = root.split_evenly((5, 2));
    for (index, (area, panel)) in panels.into_iter().zip(&data.panels).enumerate() {
        let maximum = limits.panel_maxima.get(index).copied().unwrap_or(1);
        render_panel(
            &area,
            panel,
            &data.phases,
            limits.elapsed_ms,
            maximum,
            index == 0,
        )?;
    }
    root.present()
        .context("failed to finish telemetry dashboard")
}

fn render_panel<Backend: DrawingBackend>(
    area: &DrawingArea<Backend, Shift>,
    panel: &Panel,
    phases: &[PhaseMarker],
    elapsed_ms: u64,
    maximum: u64,
    show_phase_labels: bool,
) -> Result<()>
where
    Backend::ErrorType: 'static,
{
    let mut chart = ChartBuilder::on(area)
        .caption(panel.title, ("sans-serif", 23).into_font())
        .margin(18)
        .x_label_area_size(36)
        .y_label_area_size(82)
        .build_cartesian_2d(0_u64..elapsed_ms.max(1), 0_u64..maximum.max(1))
        .context("failed to build telemetry chart")?;
    chart
        .configure_mesh()
        .bold_line_style(RGBColor(220, 224, 230).mix(0.55))
        .light_line_style(RGBColor(234, 238, 242).mix(0.45))
        .axis_style(RGBColor(87, 96, 106))
        .label_style(("sans-serif", 16).into_font())
        .x_desc("elapsed seconds")
        .y_desc(panel.unit.label())
        .x_label_formatter(&|value| format_milliseconds_as_seconds(*value))
        .y_label_formatter(&|value| panel.unit.format(*value))
        .draw()
        .context("failed to render telemetry chart axes")?;

    for marker in phases {
        if marker.elapsed_ms > elapsed_ms {
            continue;
        }
        chart
            .draw_series(once(PathElement::new(
                [(marker.elapsed_ms, 0), (marker.elapsed_ms, maximum)],
                ShapeStyle::from(&RGBColor(110, 118, 129).mix(0.35)).stroke_width(1),
            )))
            .context("failed to render a phase boundary")?;
    }

    let mut has_labels = false;
    for series in &panel.series {
        let values = if series.smooth {
            moving_average(&series.values, SMOOTHING_RADIUS)
        } else {
            series.values.clone()
        };
        let trend_has_values = values.iter().any(Option::is_some);
        let color = series.color;
        if series.smooth {
            has_labels |= draw_points(
                &mut chart,
                series.raw_values.as_ref().unwrap_or(&series.values),
                ShapeStyle::from(&series.color.mix(0.25)).stroke_width(1),
                (!trend_has_values).then_some((&series.name, color)),
            )?;
        }
        let style = ShapeStyle::from(&color).stroke_width(3);
        has_labels |= if series.points_only {
            draw_points(&mut chart, &values, style, Some((&series.name, color)))?
        } else {
            draw_segments(&mut chart, &values, style, Some((&series.name, color)))?
        };
    }
    if has_labels {
        chart
            .configure_series_labels()
            .background_style(WHITE.mix(0.85))
            .border_style(RGBColor(208, 215, 222))
            .label_font(("sans-serif", 14).into_font())
            .position(SeriesLabelPosition::UpperRight)
            .draw()
            .context("failed to render telemetry chart legend")?;
    }
    if show_phase_labels {
        for (index, marker) in phases
            .iter()
            .filter(|marker| marker.elapsed_ms <= elapsed_ms)
            .enumerate()
        {
            let row = u64::try_from(index).unwrap_or_default().min(3);
            let height = 96_u64.saturating_sub(row.saturating_mul(9));
            let horizontal = if u128::from(marker.elapsed_ms).saturating_mul(5)
                >= u128::from(elapsed_ms).saturating_mul(4)
            {
                HPos::Right
            } else {
                HPos::Left
            };
            let label_height = maximum
                .saturating_mul(height)
                .div_ceil(100)
                .max(1)
                .min(maximum);
            chart
                .draw_series(once(Text::new(
                    marker.phase.to_string(),
                    (marker.elapsed_ms, label_height),
                    ("sans-serif", 13)
                        .into_font()
                        .color(&RGBColor(87, 96, 106))
                        .pos(Pos::new(horizontal, VPos::Center)),
                )))
                .context("failed to render a phase label")?;
        }
    }
    Ok(())
}

fn draw_segments<Backend: DrawingBackend>(
    chart: &mut ChartContext<'_, Backend, Cartesian2d<RangedCoordu64, RangedCoordu64>>,
    values: &[Option<(u64, u64)>],
    style: ShapeStyle,
    label: Option<(&str, RGBColor)>,
) -> Result<bool>
where
    Backend::ErrorType: 'static,
{
    let mut labelled = false;
    for segment in contiguous_segments(values) {
        if segment.is_empty() {
            continue;
        }
        if segment.len() == 1 {
            let Some(point) = segment.first().copied() else {
                continue;
            };
            let annotation = chart
                .draw_series(once(Circle::new(point, 4, style.filled())))
                .context("failed to render a telemetry point")?;
            if !labelled && let Some((name, color)) = label {
                annotation.label(name.to_owned()).legend(move |(x, y)| {
                    PathElement::new(
                        [(x, y), (x + 24, y)],
                        ShapeStyle::from(&color).stroke_width(3),
                    )
                });
                labelled = true;
            }
            continue;
        }
        let annotation = chart
            .draw_series(LineSeries::new(segment, style))
            .context("failed to render a telemetry series")?;
        if !labelled && let Some((name, color)) = label {
            annotation.label(name.to_owned()).legend(move |(x, y)| {
                PathElement::new(
                    [(x, y), (x + 24, y)],
                    ShapeStyle::from(&color).stroke_width(3),
                )
            });
            labelled = true;
        }
    }
    Ok(labelled)
}

fn draw_points<Backend: DrawingBackend>(
    chart: &mut ChartContext<'_, Backend, Cartesian2d<RangedCoordu64, RangedCoordu64>>,
    values: &[Option<(u64, u64)>],
    style: ShapeStyle,
    label: Option<(&str, RGBColor)>,
) -> Result<bool>
where
    Backend::ErrorType: 'static,
{
    if !values.iter().any(Option::is_some) {
        return Ok(false);
    }
    let annotation = chart
        .draw_series(
            values
                .iter()
                .filter_map(|value| value.map(|point| Circle::new(point, 3, style.filled()))),
        )
        .context("failed to render telemetry points")?;
    if let Some((name, color)) = label {
        annotation.label(name.to_owned()).legend(move |(x, y)| {
            PathElement::new(
                [(x, y), (x + 24, y)],
                ShapeStyle::from(&color).stroke_width(3),
            )
        });
        Ok(true)
    } else {
        Ok(false)
    }
}

fn contiguous_segments(values: &[Option<(u64, u64)>]) -> Vec<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for value in values {
        if let Some(value) = value {
            current.push(*value);
        } else if !current.is_empty() {
            segments.push(take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn moving_average(values: &[Option<(u64, u64)>], radius: usize) -> Vec<Option<(u64, u64)>> {
    let mut output = vec![None; values.len()];
    let mut offset = 0;
    for segment in contiguous_segments(values) {
        while values.get(offset).is_some_and(Option::is_none) {
            offset = offset.saturating_add(1);
        }
        for (index, (elapsed_ms, _value)) in segment.iter().enumerate() {
            let start = index.saturating_sub(radius);
            let end = index
                .saturating_add(radius)
                .saturating_add(1)
                .min(segment.len());
            let (sum, count) = segment
                .iter()
                .skip(start)
                .take(end.saturating_sub(start))
                .fold((0_u128, 0_u128), |(sum, count), (_elapsed, value)| {
                    (sum.saturating_add(u128::from(*value)), count + 1)
                });
            if let Some(slot) = output.get_mut(offset.saturating_add(index)) {
                *slot = Some((
                    *elapsed_ms,
                    u64::try_from(sum / count.max(1)).unwrap_or(u64::MAX),
                ));
            }
        }
        offset = offset.saturating_add(segment.len());
    }
    output
}

impl Unit {
    const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU percent",
            Self::Mebibytes => "MiB",
            Self::PacketsPerSecond => "packets/s",
            Self::MegabitsPerSecond => "Mbit/s",
            Self::Multiplier => "times",
            Self::Milliseconds => "milliseconds",
            Self::Count => "items",
            Self::Percent => "percent",
        }
    }

    fn format(self, value: u64) -> String {
        match self {
            Self::Cpu => format_fixed(value, 1_000, if value < 10_000 { 2 } else { 1 }),
            Self::Multiplier => format_fixed(value, 1_000, 1),
            Self::Mebibytes => format_fixed(value, 1024 * 1024, 1),
            Self::MegabitsPerSecond => {
                format_fixed(value, 1_000_000, if value < 1_000_000 { 3 } else { 1 })
            }
            Self::PacketsPerSecond | Self::Milliseconds | Self::Count | Self::Percent => {
                value.to_string()
            }
        }
    }
}

fn padded_maximum(value: u64) -> u64 {
    if value == 0 {
        return 1;
    }
    let padded = (u128::from(value) * 11).div_ceil(10);
    u64::try_from(padded).unwrap_or(u64::MAX).max(value)
}

fn format_milliseconds_as_seconds(value: u64) -> String {
    format_fixed(value, 1_000, 1)
}

fn format_fixed(value: u64, unit: u64, decimals: u32) -> String {
    if unit == 0 {
        return value.to_string();
    }
    let scale = 10_u64.saturating_pow(decimals);
    let scaled = u128::from(value) * u128::from(scale) / u128::from(unit);
    let whole = scaled / u128::from(scale);
    let fraction = scaled % u128::from(scale);
    if decimals == 0 {
        whole.to_string()
    } else {
        format!(
            "{whole}.{fraction:0width$}",
            width = usize::try_from(decimals).unwrap_or(0)
        )
    }
}

#[cfg(test)]
#[path = "TESTS/dashboard_tests.rs"]
mod tests;
