use serde_json::json;

use super::{Side, format_delta, pair_runs, render_sides};
use crate::{
    ScenarioResult, ScenarioSpec,
    report::{RunData, SampleSet, parse_samples},
};

const BASELINE_REVISION: &str = "1111111111111111111111111111111111111111";
const COMPARISON_REVISION: &str = "2222222222222222222222222222222222222222";

#[test]
fn comparison_renders_revision_lines_and_deltas() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(1, 50)?;
    let baseline = run(spec, BASELINE_REVISION, 1_000, 10)?.with_samples(samples(20, 50_000));
    let comparison = run(spec, COMPARISON_REVISION, 1_000, 5)?.with_samples(samples(15, 40_000));

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| PASS | IDENTICAL | VALID | VALID | 1 | 0 |"));
    assert!(report.contains(BASELINE_REVISION));
    assert!(report.contains(COMPARISON_REVISION));
    assert!(report.contains("| Scenario | Profile | Expected deliveries | Duration |"));
    assert!(!report.contains("| Scenario | Profile | Expected deliveries | Duration | Contract |"));
    assert!(report.contains(
        "The Receiver delivery throughput chart is omitted because fewer than two scenarios have chartable values. The tabular metrics remain available."
    ));
    assert!(!report.contains("title \"Receiver delivery throughput\""));
    assert!(!report.contains("    bar ["));
    assert!(report.contains(
        "The SFU CPU time per million deliveries chart is omitted because fewer than two scenarios have chartable values. The tabular metrics remain available."
    ));
    assert!(report.contains("2,000.000000 CPU s/1M"));
    assert!(report.contains("1,000.000000 CPU s/1M"));
    assert!(report.contains("-1,000.000000 CPU s/1M (-50.0%)"));
    assert!(report.contains("| smoke-1r-50p | Baseline | 50 | 50 |"));
    assert!(report.contains("| smoke-1r-50p | Comparison | 50 | 50 |"));
    Ok(())
}

#[test]
fn comparison_pairs_mixed_conference_contracts() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 20, 5, 4, 10)?;
    let baseline = run(spec, BASELINE_REVISION, 10_000, 0)?;
    let comparison = run(spec, COMPARISON_REVISION, 10_000, 0)?;

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| PASS | IDENTICAL | VALID | INVALID | 1 | 0 |"));
    assert!(report.contains(
        "| mixed-conference-1x20-5a-4v-10s | opus-vp8-mixed-conference-v1 | 149,180 | 10 s |"
    ));
    assert!(report.contains("## Per-stream media load"));
    Ok(())
}

#[test]
fn comparison_chunks_scenarios_before_labels_become_dense() -> anyhow::Result<()> {
    let mut baseline = Vec::new();
    let mut comparison = Vec::new();
    for receivers in 1..=7 {
        let spec = ScenarioSpec::smoke(receivers, 50)?;
        baseline.push(run(spec, BASELINE_REVISION, 1_000, 0)?);
        comparison.push(run(spec, COMPARISON_REVISION, 1_000, 0)?);
    }

    let report = render_runs(baseline, comparison)?;

    assert!(report.contains("title \"Receiver delivery throughput (1/2)\""));
    assert!(report.contains("title \"Receiver delivery throughput (2/2)\""));
    assert!(report.contains("x-axis [\"S 5r/50p\", \"S 6r/50p\", \"S 7r/50p\"]"));
    Ok(())
}

#[test]
fn comparison_rejects_workload_contract_mismatch() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(1, 50)?;
    let baseline = run(spec, BASELINE_REVISION, 1_000, 0)?;
    let mut comparison = run(spec, COMPARISON_REVISION, 1_000, 0)?;
    comparison.result.profile = "different-profile".to_owned();

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| FAIL | MISMATCH | VALID | INVALID |"));
    assert!(report.contains("smoke-1r-50p has different workload contracts"));
    assert!(report.contains("| smoke-1r-50p | opus-fanout-smoke-v3 | 50 | 1 s |"));
    assert!(!report.contains("```mermaid"));
    Ok(())
}

#[test]
fn comparison_reports_missing_scenarios() -> anyhow::Result<()> {
    let baseline = run(ScenarioSpec::smoke(1, 50)?, BASELINE_REVISION, 1_000, 0)?;
    let comparison = run(ScenarioSpec::smoke(2, 50)?, COMPARISON_REVISION, 1_000, 0)?;

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("smoke-1r-50p is missing from the comparison"));
    assert!(report.contains("smoke-2r-50p is missing from the baseline"));
    assert!(report.contains("No scenario pair was available for comparison."));
    Ok(())
}

#[test]
fn comparison_pairs_distinct_scenarios_with_the_same_rounded_rate() -> anyhow::Result<()> {
    let first = ScenarioSpec::smoke(1, 52)?;
    let second = ScenarioSpec::smoke(1, 53)?;
    let baseline = vec![
        run(first, BASELINE_REVISION, 1_000, 0)?,
        run(second, BASELINE_REVISION, 1_000, 0)?,
    ];
    let comparison = vec![
        run(first, COMPARISON_REVISION, 1_000, 0)?,
        run(second, COMPARISON_REVISION, 1_000, 0)?,
    ];

    let pairing = pair_runs(&baseline, &comparison);

    assert_eq!(pairing.pairs.len(), 2);
    assert!(pairing.issues.is_empty());
    Ok(())
}

#[test]
fn comparison_rejects_mixed_revisions() -> anyhow::Result<()> {
    let baseline = vec![
        run(ScenarioSpec::smoke(1, 50)?, BASELINE_REVISION, 1_000, 0)?,
        run(ScenarioSpec::smoke(2, 50)?, COMPARISON_REVISION, 1_000, 0)?,
    ];
    let comparison = vec![
        run(ScenarioSpec::smoke(1, 50)?, COMPARISON_REVISION, 1_000, 0)?,
        run(ScenarioSpec::smoke(2, 50)?, COMPARISON_REVISION, 1_000, 0)?,
    ];

    let report = render_runs(baseline, comparison)?;

    assert!(report.contains("| mixed |"));
    assert!(report.contains("Baseline results do not contain one full revision SHA."));
    Ok(())
}

#[test]
fn revision_errors_do_not_rewrite_exact_delivery() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(1, 50)?;
    let baseline = run(spec, BASELINE_REVISION, 1_000, 0)?;
    let comparison = run(spec, BASELINE_REVISION, 1_000, 0)?;

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| PASS | IDENTICAL | INVALID | INVALID |"));
    assert!(report.contains("Baseline and comparison revisions are identical."));
    Ok(())
}

#[test]
fn exact_work_survives_invalid_performance_evidence() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(1, 50)?;
    let baseline = run(spec, BASELINE_REVISION, 1_000, 0)?;
    let comparison = run(spec, COMPARISON_REVISION, 1_000, 21)?;

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| PASS | IDENTICAL | VALID | INVALID |"));
    assert!(report.contains("The SFU CPU time per million deliveries graph has no paired data."));
    Ok(())
}

#[test]
fn missing_traffic_counters_invalidate_performance_evidence() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(1, 50)?;
    let baseline = run(spec, BASELINE_REVISION, 1_000, 0)?.with_samples(samples(20, 50_000));
    let comparison =
        run(spec, COMPARISON_REVISION, 1_000, 0)?.with_samples(samples_without_traffic(15, 40_000));

    let report = render_runs(vec![baseline], vec![comparison])?;

    assert!(report.contains("| PASS | IDENTICAL | VALID | INVALID |"));
    assert!(report.contains("| SFU forwarded packets | 50 packets/s | n/a | n/a |"));
    Ok(())
}

#[test]
fn signed_delta_handles_zero_baselines() {
    assert_eq!(
        format_delta(Some(0), Some(10), |value| value.to_string()),
        "+10 (n/a)"
    );
    assert_eq!(
        format_delta(Some(10), Some(5), |value| value.to_string()),
        "-5 (-50.0%)"
    );
    assert_eq!(
        format_delta(Some(10), Some(10), |value| value.to_string()),
        "0 (0.0%)"
    );
}

fn run(
    spec: ScenarioSpec,
    revision: &str,
    elapsed_ms: u64,
    max_send_lag_ms: u64,
) -> anyhow::Result<RunData> {
    let plan = spec.plan()?;
    let result = serde_json::from_value::<ScenarioResult>(json!({
        "schemaVersion": 4,
        "profile": spec.profile(),
        "oSfuRevision": revision,
        "scenario": spec,
        "serverPolicy": {
            "mediaWorkers": 1,
            "roomSize": spec.peers_per_room(),
            "maxPreAuthWebsocketSessionsPerOrigin": spec.room_count() * spec.peers_per_room(),
            "maxActiveAudioSpeakers": spec.active_audio_speakers(),
            "maxVideoDownloadsPerReceiver": 10,
            "maxBitrateInBps": 8_000_000,
            "maxBitrateOutBps": 10_000_000
        },
        "plan": plan,
        "offeredPackets": plan.offered_packets,
        "offeredPayloadBytes": plan.offered_payload_bytes,
        "deliveredPackets": plan.expected_deliveries,
        "deliveredPayloadBytes": plan.expected_delivery_payload_bytes,
        "correctness": {
            "missingPackets": 0,
            "duplicatePackets": 0,
            "outOfOrderPackets": 0,
            "unexpectedPackets": 0,
            "payloadMismatches": 0
        },
        "elapsedMs": elapsed_ms,
        "maxSendLagMs": max_send_lag_ms
    }))?;
    Ok(RunData {
        source: revision.to_owned(),
        result,
        samples: None,
    })
}

fn samples(final_ticks: u64, cpu_percent_milli: u64) -> SampleSet {
    parse_samples(&format!(
        r#"
{{"elapsedMs":0,"status":"sample","clockTicksPerSecond":100,"server":{{"cpuTicks":10,"rssBytes":1048576,"startTimeTicks":5}},"rtc":{{"cpuTicks":20,"rssBytes":2097152,"startTimeTicks":7}},"traffic":{{"forwardedLocalRtc":{{"packets":0}},"egress":{{"payloadBytes":0}}}},"workers":[{{"packetLoopDelayMs":2}}]}}
{{"elapsedMs":1000,"status":"sample","clockTicksPerSecond":100,"serverCpuPercentMilli":{cpu_percent_milli},"rtcCpuPercentMilli":25000,"server":{{"cpuTicks":{final_ticks},"rssBytes":3145728,"startTimeTicks":5}},"rtc":{{"cpuTicks":30,"rssBytes":4194304,"startTimeTicks":7}},"traffic":{{"forwardedLocalRtc":{{"packets":50}},"egress":{{"payloadBytes":8000}}}},"workers":[{{"packetLoopDelayMs":7}}]}}
"#
    ))
}

fn samples_without_traffic(final_ticks: u64, cpu_percent_milli: u64) -> SampleSet {
    parse_samples(&format!(
        r#"
{{"elapsedMs":0,"status":"sample","clockTicksPerSecond":100,"server":{{"cpuTicks":10,"rssBytes":1048576,"startTimeTicks":5}},"rtc":{{"cpuTicks":20,"rssBytes":2097152,"startTimeTicks":7}},"workers":[{{"packetLoopDelayMs":2}}]}}
{{"elapsedMs":1000,"status":"sample","clockTicksPerSecond":100,"serverCpuPercentMilli":{cpu_percent_milli},"rtcCpuPercentMilli":25000,"server":{{"cpuTicks":{final_ticks},"rssBytes":3145728,"startTimeTicks":5}},"rtc":{{"cpuTicks":30,"rssBytes":4194304,"startTimeTicks":7}},"workers":[{{"packetLoopDelayMs":7}}]}}
"#
    ))
}

trait RunTestExt {
    fn with_samples(self, samples: SampleSet) -> Self;
}

impl RunTestExt for RunData {
    fn with_samples(mut self, samples: SampleSet) -> Self {
        self.samples = Some(samples);
        self
    }
}

fn render_runs(baseline: Vec<RunData>, comparison: Vec<RunData>) -> anyhow::Result<String> {
    render_sides(
        &Side {
            runs: baseline,
            failures: Vec::new(),
        },
        &Side {
            runs: comparison,
            failures: Vec::new(),
        },
        None,
    )
}
