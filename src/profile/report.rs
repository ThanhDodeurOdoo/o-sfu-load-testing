use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fmt::Write as _,
    fs::{self, File},
    io::ErrorKind,
    path::Path,
    process::{Command, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use super::{
    CAPTURE_FILE, ENVIRONMENT_FILE, FLAMEGRAPH_FILE, FOLDED_FILE, PERF_DATA_FILE,
    PROFILE_READY_FILE, capture::CaptureMetadata,
};
use crate::report::{
    RunData, ensure_summary_size, escape_table, load_run, scenario_label, validate_artifact_url,
    validate_flamegraph_url, validate_run,
};

const CAPTURE_LIMIT_BYTES: u64 = 64 * 1024;
const FOLDED_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 8;
const MAX_FOLDED_LINES: usize = 1_000_000;
const MAX_HOT_STACKS: usize = 10;
const MAX_ROWS: usize = 15;
const PREPARED_FILES: [&str; 16] = [
    PROFILE_READY_FILE,
    ENVIRONMENT_FILE,
    "perf.script",
    "perf-script.stderr.log",
    FOLDED_FILE,
    "collapse.stderr.log",
    FLAMEGRAPH_FILE,
    "flamegraph.stderr.log",
    "perf-header.txt",
    "perf-header.stderr.log",
    "hotspots-self.txt",
    "hotspots-self.txt.stderr.log",
    "hotspots-inclusive.txt",
    "hotspots-inclusive.txt.stderr.log",
    "threads.txt",
    "threads.txt.stderr.log",
];

struct FoldedProfile {
    total_samples: u64,
    stack_samples: BTreeMap<String, u64>,
    self_samples: BTreeMap<String, u64>,
    inclusive_samples: BTreeMap<String, u64>,
    thread_samples: BTreeMap<String, u64>,
    kernel_samples: u64,
    unresolved_leaf_samples: u64,
    unresolved_stack_samples: u64,
    malformed_lines: usize,
    diagnostics: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentMetadata {
    schema_version: u32,
    cpu_model: String,
    logical_cpus: usize,
    kernel: String,
    perf_version: String,
    inferno_version: String,
    rustc_version: String,
    runner_image: String,
    perf_event_max_stack: String,
}

struct PreparedProfile {
    run: RunData,
    capture: CaptureMetadata,
    environment: EnvironmentMetadata,
    profile: FoldedProfile,
}

/// Converts a captured `perf.data` into folded stacks, an Inferno flamegraph
/// and raw `perf report` views.
///
/// # Errors
///
/// Returns an error when `perf` cannot decode the capture, stack collapsing or
/// flamegraph generation fails or no samples remain.
pub fn prepare(input: &Path) -> Result<()> {
    ensure!(input.is_dir(), "profile input must be a directory");
    clear_prepared(input)?;
    let perf_data = input.join(PERF_DATA_FILE);
    ensure_nonempty(&perf_data)?;
    let perf_script = input.join("perf.script");
    run_perf(
        &["script", "--fields", "sw:-period", "--input"],
        &perf_data,
        &perf_script,
        &input.join("perf-script.stderr.log"),
    )?;

    let folded = input.join(FOLDED_FILE);
    collapse_stacks(input, &perf_script, &folded)?;

    let payload = read_limited(&folded, FOLDED_LIMIT_BYTES)?;
    let profile = FoldedProfile::parse(&payload)?;
    ensure!(
        profile.total_samples > 0,
        "perf capture contains no samples"
    );
    render_flamegraph(input, &folded)?;

    run_perf(
        &["report", "--stdio", "--header-only", "--input"],
        &perf_data,
        &input.join("perf-header.txt"),
        &input.join("perf-header.stderr.log"),
    )?;
    run_perf_report(
        input,
        &perf_data,
        "hotspots-self.txt",
        &["--no-children", "--sort", "comm,dso,symbol"],
    )?;
    run_perf_report(
        input,
        &perf_data,
        "hotspots-inclusive.txt",
        &["--children", "--sort", "comm,dso,symbol"],
    )?;
    run_perf_report(
        input,
        &perf_data,
        "threads.txt",
        &["--no-children", "--sort", "comm,pid"],
    )?;
    write_environment(input)?;
    fs::write(input.join(PROFILE_READY_FILE), b"ready\n")
        .context("failed to mark the CPU profile as complete")
}

fn clear_prepared(input: &Path) -> Result<()> {
    for name in PREPARED_FILES {
        let path = input.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

fn write_environment(input: &Path) -> Result<()> {
    let environment = EnvironmentMetadata {
        schema_version: 1,
        cpu_model: cpu_model(),
        logical_cpus: thread::available_parallelism().map_or(0, usize::from),
        kernel: command_value("uname", &["-srmo"]),
        perf_version: command_value("perf", &["version"]),
        inferno_version: env::var("O_SFU_LOAD_INFERNO_VERSION")
            .unwrap_or_else(|_error| "unreported".to_owned()),
        rustc_version: command_value("rustc", &["--version"]),
        runner_image: runner_image(),
        perf_event_max_stack: system_value("/proc/sys/kernel/perf_event_max_stack"),
    };
    let payload = serde_json::to_vec_pretty(&environment)
        .context("failed to encode the profiling environment")?;
    fs::write(input.join(ENVIRONMENT_FILE), payload)
        .context("failed to write the profiling environment")
}

fn cpu_model() -> String {
    let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") else {
        return "unavailable".to_owned();
    };
    ["model name", "Hardware"]
        .into_iter()
        .find_map(|field| {
            cpuinfo.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == field).then(|| value.trim().to_owned())
            })
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn command_value(program: &str, arguments: &[&str]) -> String {
    let Ok(output) = Command::new(program).args(arguments).output() else {
        return "unavailable".to_owned();
    };
    if !output.status.success() {
        return "unavailable".to_owned();
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        "unavailable".to_owned()
    } else {
        value
    }
}

fn system_value(path: &str) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn runner_image() -> String {
    let os = env::var("ImageOS").ok();
    let version = env::var("ImageVersion").ok();
    match (os, version) {
        (Some(os), Some(version)) => format!("{os} {version}"),
        (Some(os), None) => os,
        (None, Some(version)) => version,
        (None, None) => "non-GitHub environment".to_owned(),
    }
}

fn load_prepared(input: &Path) -> Result<PreparedProfile> {
    for name in [
        PROFILE_READY_FILE,
        PERF_DATA_FILE,
        FLAMEGRAPH_FILE,
        "perf-header.txt",
        "hotspots-self.txt",
        "hotspots-inclusive.txt",
        "threads.txt",
    ] {
        ensure_nonempty(&input.join(name))?;
    }
    let run = load_run(input)?;
    let capture_payload = read_limited(&input.join(CAPTURE_FILE), CAPTURE_LIMIT_BYTES)?;
    let capture = serde_json::from_str::<CaptureMetadata>(&capture_payload)
        .context("failed to decode profile metadata")?;
    ensure!(
        capture.schema_version == 1,
        "unsupported profile metadata schema"
    );
    let environment_payload = read_limited(&input.join(ENVIRONMENT_FILE), CAPTURE_LIMIT_BYTES)?;
    let environment = serde_json::from_str::<EnvironmentMetadata>(&environment_payload)
        .context("failed to decode profiling environment")?;
    ensure!(
        environment.schema_version == 1,
        "unsupported profiling environment schema"
    );
    let folded = read_limited(&input.join(FOLDED_FILE), FOLDED_LIMIT_BYTES)?;
    let profile = FoldedProfile::parse(&folded)?;
    ensure!(
        profile.total_samples > 0,
        "profile contains no valid samples"
    );
    Ok(PreparedProfile {
        run,
        capture,
        environment,
        profile,
    })
}

/// Renders the CPU profile summary from one prepared profile directory.
///
/// # Errors
///
/// Returns an error when profile inputs are missing or invalid, the artifact
/// URLs are unsafe for Markdown or the summary exceeds GitHub's one MiB limit.
pub fn render(
    input: &Path,
    artifact_url: Option<&str>,
    flamegraph_url: Option<&str>,
) -> Result<String> {
    validate_artifact_url(artifact_url)?;
    validate_flamegraph_url(flamegraph_url)?;
    let report = load_prepared(input)?;
    let mut output = String::new();
    render_overview(&mut output, &report, artifact_url, flamegraph_url)?;
    render_breakdown(&mut output, &report.profile)?;
    render_diagnostics(&mut output, &report.profile)?;
    ensure_summary_size(&output)?;
    Ok(output)
}

fn render_overview(
    output: &mut String,
    report: &PreparedProfile,
    artifact_url: Option<&str>,
    flamegraph_url: Option<&str>,
) -> Result<()> {
    writeln!(output, "# o-sfu CPU profile\n")?;
    writeln!(
        output,
        "| Profile | Exact RTC work | Scenario | o-sfu revision | Event | Unwind | Requested frequency | Capture | Samples | Unresolved leaf |"
    )?;
    writeln!(
        output,
        "| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: |"
    )?;
    writeln!(
        output,
        "| {} | {} | {} | {} | {} | {} | {} Hz | {} | {} | {} |\n",
        if report.profile.malformed_lines == 0 {
            "AVAILABLE"
        } else {
            "INCOMPLETE"
        },
        if validate_run(&report.run).is_ok() {
            "PASS"
        } else {
            "FAIL"
        },
        scenario_label(report.run.result.scenario),
        escape_table(report.run.result.o_sfu_revision.as_deref().unwrap_or("n/a")),
        escape_table(&report.capture.event),
        escape_table(&report.capture.call_graph),
        report.capture.frequency_hz,
        format_duration(report.capture.duration_ms),
        grouped(report.profile.total_samples),
        format_count_percent(
            report.profile.unresolved_leaf_samples,
            report.profile.total_samples
        )
    )?;
    writeln!(
        output,
        "This is a dedicated qualitative replay built with frame pointers. Sampling starts after server readiness and covers peer setup, warmup, measured traffic and drain. Sampling overhead does not affect the authoritative nightly measurements.\n"
    )?;
    writeln!(
        output,
        "Flamegraph width represents sample share and horizontal position is not time. Stack depth grows vertically.\n"
    )?;
    if let Some(url) = flamegraph_url {
        let target = artifact_url.unwrap_or(url);
        writeln!(output, "[![o-sfu CPU flamegraph]({url})]({target})\n")?;
        writeln!(
            output,
            "The embedded PNG is a static preview. The artifact retains Inferno's interactive SVG with stack search and zoom controls.\n"
        )?;
    }
    if let Some(url) = artifact_url {
        writeln!(
            output,
            "[Download the interactive flamegraph, perf.data, folded stacks and raw reports]({url})\n"
        )?;
    } else {
        writeln!(
            output,
            "The artifact contains `flamegraph.svg`, `perf.data`, folded stacks and raw reports.\n"
        )?;
    }
    render_environment(output, &report.environment)
}

fn render_environment(output: &mut String, environment: &EnvironmentMetadata) -> Result<()> {
    let logical_cpus = environment.logical_cpus.to_string();
    writeln!(output, "## Runner context\n")?;
    writeln!(output, "| Property | Value |")?;
    writeln!(output, "| --- | --- |")?;
    for (property, value) in [
        ("CPU model", environment.cpu_model.as_str()),
        ("Logical CPUs", logical_cpus.as_str()),
        ("Kernel", environment.kernel.as_str()),
        ("perf", environment.perf_version.as_str()),
        ("Inferno", environment.inferno_version.as_str()),
        ("Rust", environment.rustc_version.as_str()),
        ("Runner image", environment.runner_image.as_str()),
        (
            "Maximum stack depth",
            environment.perf_event_max_stack.as_str(),
        ),
    ] {
        writeln!(output, "| {property} | {} |", escape_table(value))?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_breakdown(output: &mut String, profile: &FoldedProfile) -> Result<()> {
    writeln!(output, "## Sample mode\n")?;
    writeln!(output, "| Leaf mode | Samples | Share |")?;
    writeln!(output, "| --- | ---: | ---: |")?;
    render_count_row(
        output,
        "Kernel",
        profile.kernel_samples,
        profile.total_samples,
    )?;
    render_count_row(
        output,
        "Non-kernel leaf",
        profile.total_samples.saturating_sub(profile.kernel_samples),
        profile.total_samples,
    )?;
    render_count_row(
        output,
        "Unresolved leaf",
        profile.unresolved_leaf_samples,
        profile.total_samples,
    )?;
    render_count_row(
        output,
        "Partially symbolized stack",
        profile.unresolved_stack_samples,
        profile.total_samples,
    )?;
    writeln!(
        output,
        "\nKernel and non-kernel leaf rows partition the samples. Unresolved leaf measures self cost that cannot be attributed to a symbol. Partially symbolized stacks can include address-only system-library roots and overlap the other rows.\n"
    )?;

    render_ranked(
        output,
        "Thread sample share",
        "Thread",
        &profile.thread_samples,
        profile.total_samples,
        MAX_ROWS,
        true,
    )?;
    writeln!(
        output,
        "Self cost attributes each sample to its leaf frame and partitions the sample set.\n"
    )?;
    render_ranked(
        output,
        "Hottest leaf symbols",
        "Symbol",
        &profile.self_samples,
        profile.total_samples,
        MAX_ROWS,
        true,
    )?;
    writeln!(
        output,
        "Inclusive cost counts a frame once when it appears anywhere in a sampled stack. Rows overlap and must not be summed.\n"
    )?;
    let inclusive_samples = profile
        .inclusive_samples
        .iter()
        .filter(|(frame, _count)| informative_inclusive_frame(frame))
        .map(|(frame, count)| (frame.clone(), *count))
        .collect::<BTreeMap<_, _>>();
    render_ranked(
        output,
        "Hottest inclusive frames",
        "Frame",
        &inclusive_samples,
        profile.total_samples,
        MAX_ROWS,
        false,
    )?;
    writeln!(
        output,
        "The summary ranking omits unresolved address frames and process bootstrap wrappers. The flamegraph and raw perf reports retain them.\n"
    )?;
    render_hot_stacks(output, profile)?;
    Ok(())
}

fn render_diagnostics(output: &mut String, profile: &FoldedProfile) -> Result<()> {
    if profile.malformed_lines > 0 {
        writeln!(output, "## Profile diagnostics\n")?;
        writeln!(
            output,
            "{} folded-stack lines were invalid. Valid samples remain reported above.\n",
            grouped(u64::try_from(profile.malformed_lines).unwrap_or(u64::MAX))
        )?;
        for diagnostic in &profile.diagnostics {
            writeln!(output, "- {}", escape_table(diagnostic))?;
        }
        writeln!(output)?;
    }
    writeln!(
        output,
        "Raw `hotspots-self.txt`, `hotspots-inclusive.txt`, `threads.txt` and `perf-header.txt` retain perf's DSO and symbol views.\n"
    )?;
    Ok(())
}

/// Writes one prepared CPU profile summary.
///
/// # Errors
///
/// Returns an error when rendering, directory creation or persistence fails.
pub fn write(
    input: &Path,
    output: &Path,
    artifact_url: Option<&str>,
    flamegraph_url: Option<&str>,
) -> Result<()> {
    let summary = render(input, artifact_url, flamegraph_url)?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("failed to create the profile report directory")?;
    }
    fs::write(output, summary).context("failed to write the profile report")
}

impl FoldedProfile {
    fn parse(payload: &str) -> Result<Self> {
        let mut profile = Self {
            total_samples: 0,
            stack_samples: BTreeMap::new(),
            self_samples: BTreeMap::new(),
            inclusive_samples: BTreeMap::new(),
            thread_samples: BTreeMap::new(),
            kernel_samples: 0,
            unresolved_leaf_samples: 0,
            unresolved_stack_samples: 0,
            malformed_lines: 0,
            diagnostics: Vec::new(),
        };
        for (line_index, line) in payload.lines().enumerate() {
            if line_index >= MAX_FOLDED_LINES {
                return Err(anyhow!(
                    "folded profile exceeds its {MAX_FOLDED_LINES} line limit"
                ));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Err(error) = profile.add_line(line) {
                profile.malformed_lines = profile.malformed_lines.saturating_add(1);
                if profile.diagnostics.len() < MAX_DIAGNOSTICS {
                    profile
                        .diagnostics
                        .push(format!("line {}: {error}", line_index + 1));
                }
            }
        }
        Ok(profile)
    }

    fn add_line(&mut self, line: &str) -> Result<()> {
        let (stack, count) = line.rsplit_once(' ').context("missing sample count")?;
        let count = count
            .parse::<u64>()
            .context("sample count is not an integer")?;
        ensure!(count > 0, "sample count must be positive");
        let frames = stack
            .split(';')
            .filter(|frame| !frame.is_empty())
            .collect::<Vec<_>>();
        let leaf = frames.last().context("stack contains no frames")?;
        let thread = frames.first().context("stack contains no thread root")?;
        self.total_samples = self
            .total_samples
            .checked_add(count)
            .context("total sample count overflowed")?;
        add_count(&mut self.stack_samples, stack, count)?;
        add_count(&mut self.self_samples, leaf, count)?;
        add_count(&mut self.thread_samples, thread, count)?;
        let inclusive_start = usize::from(frames.len() > 1);
        let inclusive_frames = frames.iter().skip(inclusive_start);
        for frame in inclusive_frames.copied().collect::<BTreeSet<_>>() {
            add_count(&mut self.inclusive_samples, frame, count)?;
        }
        if leaf.ends_with("_[k]") {
            self.kernel_samples = self
                .kernel_samples
                .checked_add(count)
                .context("kernel sample count overflowed")?;
        }
        if unresolved(leaf) {
            self.unresolved_leaf_samples = self
                .unresolved_leaf_samples
                .checked_add(count)
                .context("unresolved leaf sample count overflowed")?;
        }
        if frames.iter().any(|frame| unresolved(frame)) {
            self.unresolved_stack_samples = self
                .unresolved_stack_samples
                .checked_add(count)
                .context("unresolved stack sample count overflowed")?;
        }
        Ok(())
    }
}

fn collapse_stacks(input: &Path, perf_script: &Path, folded: &Path) -> Result<()> {
    let mut command = Command::new("inferno-collapse-perf");
    command
        .args(["--kernel", "--tid", "--addrs"])
        .arg(perf_script);
    run_to_file(
        &mut command,
        folded,
        &input.join("collapse.stderr.log"),
        "inferno-collapse-perf",
    )
}

fn render_flamegraph(input: &Path, folded: &Path) -> Result<()> {
    let run = load_run(input)?;
    let title = format!("o-sfu CPU: {}", scenario_label(run.result.scenario));
    let subtitle = format!(
        "requested {} Hz cpu-clock, o-sfu {}",
        super::FREQUENCY_HZ,
        run.result.o_sfu_revision.as_deref().unwrap_or("unknown")
    );
    let output = input.join(FLAMEGRAPH_FILE);
    let mut command = Command::new("inferno-flamegraph");
    command
        .arg("--deterministic")
        .arg("--width")
        .arg("1800")
        .arg("--minwidth")
        .arg("0.05")
        .arg("--title")
        .arg(title)
        .arg("--subtitle")
        .arg(subtitle)
        .arg(folded);
    run_to_file(
        &mut command,
        &output,
        &input.join("flamegraph.stderr.log"),
        "inferno-flamegraph",
    )
}

fn run_perf(prefix: &[&str], input: &Path, output: &Path, stderr: &Path) -> Result<()> {
    let stdout =
        File::create(output).with_context(|| format!("failed to create {}", output.display()))?;
    let stderr_file =
        File::create(stderr).with_context(|| format!("failed to create {}", stderr.display()))?;
    let mut command = Command::new("perf");
    command.args(prefix).arg(input);
    run_to_file_with_handles(&mut command, stdout, stderr_file, output, "perf")
}

fn run_perf_report(
    input: &Path,
    perf_data: &Path,
    output_name: &str,
    options: &[&str],
) -> Result<()> {
    let output = input.join(output_name);
    let stderr = input.join(format!("{output_name}.stderr.log"));
    let stdout =
        File::create(&output).with_context(|| format!("failed to create {}", output.display()))?;
    let stderr_file =
        File::create(&stderr).with_context(|| format!("failed to create {}", stderr.display()))?;
    let mut command = Command::new("perf");
    command
        .arg("report")
        .arg("--stdio")
        .arg("--input")
        .arg(perf_data)
        .args(options)
        .arg("--percent-limit")
        .arg("0.25");
    run_to_file_with_handles(&mut command, stdout, stderr_file, &output, "perf report")
}

fn run_to_file(command: &mut Command, output: &Path, stderr: &Path, label: &str) -> Result<()> {
    let stdout =
        File::create(output).with_context(|| format!("failed to create {}", output.display()))?;
    let stderr_file =
        File::create(stderr).with_context(|| format!("failed to create {}", stderr.display()))?;
    run_to_file_with_handles(command, stdout, stderr_file, output, label)
}

fn run_to_file_with_handles(
    command: &mut Command,
    stdout: File,
    stderr: File,
    output: &Path,
    label: &str,
) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()
        .with_context(|| format!("failed to run {label}"))?;
    ensure!(status.success(), "{label} exited with {status}");
    ensure_nonempty(output)
}

fn render_ranked(
    output: &mut String,
    title: &str,
    name: &str,
    values: &BTreeMap<String, u64>,
    total: u64,
    limit: usize,
    partition: bool,
) -> Result<()> {
    writeln!(output, "## {title}\n")?;
    writeln!(output, "| Rank | {name} | Samples | Share |")?;
    writeln!(output, "| ---: | --- | ---: | ---: |")?;
    let ranked = ranked(values, limit);
    let shown = ranked
        .iter()
        .fold(0_u64, |sum, (_label, count)| sum.saturating_add(*count));
    for (index, (label, count)) in ranked.into_iter().enumerate() {
        writeln!(
            output,
            "| {} | {} | {} | {} |",
            index + 1,
            escape_table(label),
            grouped(count),
            format_percent(count, total)
        )?;
    }
    if partition && shown < total {
        writeln!(
            output,
            "| Other | Remaining entries | {} | {} |",
            grouped(total - shown),
            format_percent(total - shown, total)
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_hot_stacks(output: &mut String, profile: &FoldedProfile) -> Result<()> {
    writeln!(output, "## Hottest stack paths\n")?;
    writeln!(
        output,
        "| Rank | Thread | Leaf-side stack | Samples | Share |"
    )?;
    writeln!(output, "| ---: | --- | --- | ---: | ---: |")?;
    for (index, (stack, count)) in ranked(&profile.stack_samples, MAX_HOT_STACKS)
        .into_iter()
        .enumerate()
    {
        let (thread, stack) = display_stack(stack, 240);
        writeln!(
            output,
            "| {} | {} | {} | {} | {} |",
            index + 1,
            escape_table(thread),
            escape_table(&stack),
            grouped(count),
            format_percent(count, profile.total_samples)
        )?;
    }
    writeln!(output)?;
    Ok(())
}

fn render_count_row(output: &mut String, label: &str, count: u64, total: u64) -> Result<()> {
    writeln!(
        output,
        "| {label} | {} | {} |",
        grouped(count),
        format_percent(count, total)
    )?;
    Ok(())
}

fn ranked(values: &BTreeMap<String, u64>, limit: usize) -> Vec<(&String, u64)> {
    let mut ranked = values
        .iter()
        .map(|(label, count)| (label, *count))
        .collect::<Vec<_>>();
    ranked.sort_unstable_by(|(left_label, left_count), (right_label, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_label.cmp(right_label))
    });
    ranked.truncate(limit);
    ranked
}

fn add_count(values: &mut BTreeMap<String, u64>, label: &str, count: u64) -> Result<()> {
    let value = values.entry(label.to_owned()).or_default();
    *value = value
        .checked_add(count)
        .context("sample count overflowed")?;
    Ok(())
}

fn unresolved(frame: &str) -> bool {
    frame.contains("[unknown]")
        || frame.starts_with("0x")
        || (frame.starts_with('[') && frame.contains(" <"))
}

fn informative_inclusive_frame(frame: &str) -> bool {
    !unresolved(frame)
        && !matches!(
            frame,
            "<std::sys::thread::unix::Thread>::new::thread_start"
                | "core::ops::function::FnOnce::call_once{{vtable.shim}}"
                | "std::sys::backtrace::__rust_begin_short_backtrace"
        )
}

fn read_limited(path: &Path, limit: u64) -> Result<String> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.len() <= limit,
        "{} exceeds its {} byte limit",
        path.display(),
        limit
    );
    let payload = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        u64::try_from(payload.len()).unwrap_or(u64::MAX) <= limit,
        "{} grew beyond its {} byte limit",
        path.display(),
        limit
    );
    String::from_utf8(payload).with_context(|| format!("{} is not UTF-8", path.display()))
}

fn ensure_nonempty(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(metadata.len() > 0, "{} is empty", path.display());
    Ok(())
}

fn format_count_percent(count: u64, total: u64) -> String {
    format!("{} ({})", grouped(count), format_percent(count, total))
}

fn format_percent(count: u64, total: u64) -> String {
    if total == 0 {
        return "n/a".to_owned();
    }
    let basis_points = u128::from(count) * 10_000 / u128::from(total);
    format!("{}.{:02}%", basis_points / 100, basis_points % 100)
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms} ms")
    } else {
        format!("{}.{:03} s", duration_ms / 1_000, duration_ms % 1_000)
    }
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

fn display_stack(stack: &str, max_chars: usize) -> (&str, String) {
    const OMITTED: &str = "... -> ";

    let mut frames = stack.split(';');
    let thread = frames.next().unwrap_or("n/a");
    let frames = frames.collect::<Vec<_>>();
    let full = frames.join(" -> ");
    if full.chars().count() <= max_chars {
        return (thread, full);
    }

    let available = max_chars.saturating_sub(OMITTED.len());
    let mut kept = Vec::new();
    let mut length = 0_usize;
    for frame in frames.iter().rev() {
        let separator = usize::from(!kept.is_empty()) * " -> ".len();
        let next_length = length
            .saturating_add(separator)
            .saturating_add(frame.chars().count());
        if next_length > available {
            break;
        }
        kept.push(*frame);
        length = next_length;
    }
    kept.reverse();
    if kept.is_empty() {
        let leaf = frames.last().copied().unwrap_or("n/a");
        let leaf = leaf.chars().take(available).collect::<String>();
        return (thread, format!("{OMITTED}{leaf}"));
    }
    (thread, format!("{OMITTED}{}", kept.join(" -> ")))
}

#[cfg(test)]
#[path = "TESTS/report_tests.rs"]
mod tests;
