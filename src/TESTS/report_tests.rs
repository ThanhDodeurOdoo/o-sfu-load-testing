use serde_json::json;

use super::{
    GITHUB_SUMMARY_LIMIT_BYTES, LoadFailure, RunData, SampleSet, TelemetrySummary, bar,
    delivery_rate, ensure_summary_size, escape_table, format_mebibytes, parse_samples,
    render_report, validate_artifact_url,
};
use crate::{ScenarioResult, ScenarioSpec};

#[test]
fn report_sorts_workloads_and_uses_one_delivery_scale() -> anyhow::Result<()> {
    let large = run(ScenarioSpec::audio_mesh(1, 10, 1)?, None, 0)?;
    let small = run(ScenarioSpec::smoke(1, 50)?, None, 0)?;

    let report = render_runs(vec![large, small], None)?;
    let smoke_position = report
        .find("smoke-1r-50p")
        .ok_or_else(|| anyhow::anyhow!("smoke workload is missing"))?;
    let mesh_position = report
        .find("audio-mesh-1x10-1s")
        .ok_or_else(|| anyhow::anyhow!("mesh workload is missing"))?;

    assert!(smoke_position < mesh_position);
    assert!(report.contains("[=...............................] 50"));
    assert!(report.contains("[================================] 4,500"));
    assert!(report.contains("4,500/s"));
    Ok(())
}

#[test]
fn report_marks_delivery_discrepancies_and_send_lag() -> anyhow::Result<()> {
    let mut failed = run(ScenarioSpec::smoke(2, 50)?, Some(99), 1)?;
    failed.result.max_send_lag_ms = 37;

    let report = render_runs(vec![failed], None)?;

    assert!(report.contains("| FAIL | 1 | 0 | n/a | deadbeef |"));
    assert!(report.contains("Performance samples: **INVALID**"));
    assert!(report.contains("| 37 ms |"));
    assert!(report.contains("[################################] 99"));
    assert!(report.contains("| smoke-2r-50p | 1 | 0 | 0 | 0 | 0 | 1 |"));
    Ok(())
}

#[test]
fn malformed_input_does_not_hide_valid_results() -> anyhow::Result<()> {
    let report = render_report(
        vec![run(ScenarioSpec::smoke(1, 50)?, None, 0)?],
        vec![LoadFailure {
            source: "bad|input".to_owned(),
            error: "invalid [result](https://example.com)".to_owned(),
        }],
        None,
    )?;

    assert!(report.contains("| FAIL | 1 | 1 | n/a | deadbeef |"));
    assert!(report.contains("## Input failures"));
    assert!(report.contains("bad&#124;input"));
    assert!(report.contains("&#91;result&#93;&#40;https://example.com&#41;"));
    assert!(report.contains("## Exact delivery"));
    Ok(())
}

#[test]
fn report_survives_when_every_input_fails() -> anyhow::Result<()> {
    let report = render_report(
        Vec::new(),
        vec![LoadFailure {
            source: "missing/result.json".to_owned(),
            error: "file is missing".to_owned(),
        }],
        None,
    )?;

    assert!(report.contains("| FAIL | 0 | 1 | n/a | n/a |"));
    assert!(report.contains("No valid result files were available."));
    Ok(())
}

#[test]
fn deserialized_scenario_bounds_are_revalidated() -> anyhow::Result<()> {
    let invalid = ScenarioSpec::Smoke {
        receivers: 0,
        packets: 50,
    };
    assert!(invalid.validate().is_err());
    let mut run = run(ScenarioSpec::smoke(1, 50)?, None, 0)?;
    run.result.scenario = invalid;

    let report = render_runs(vec![run], None)?;

    assert!(report.contains("| FAIL |"));
    assert!(report.contains("receivers must be between 1 and 99"));
    Ok(())
}

#[test]
fn telemetry_surfaces_failures_and_high_load_metrics() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"status":"sample","clockTicksPerSecond":100,"server":{"cpuTicks":10,"rssBytes":1048576,"startTimeTicks":5},"rtc":{"cpuTicks":20,"rssBytes":2097152,"startTimeTicks":7},"traffic":{"forwardedLocalRtc":{"packets":0},"egress":{"payloadBytes":0}},"workers":[{"packetLoopDelayMs":2}]}
not-json
{"elapsedMs":1500,"status":"error","message":"scrape failed"}
{"elapsedMs":1000,"status":"sample","clockTicksPerSecond":100,"serverCpuPercentMilli":50000,"server":{"cpuTicks":25,"rssBytes":2097152,"startTimeTicks":5},"rtc":{"cpuTicks":30,"rssBytes":3145728,"startTimeTicks":7},"traffic":{"forwardedLocalRtc":{"packets":40},"egress":{"payloadBytes":12000}},"workers":[{"packetLoopDelayMs":7}]}
{"elapsedMs":2000,"status":"sample","clockTicksPerSecond":100,"serverCpuPercentMilli":100000,"server":{"cpuTicks":40,"rssBytes":3145728,"startTimeTicks":5},"rtc":{"cpuTicks":40,"rssBytes":4194304,"startTimeTicks":7},"traffic":{"forwardedLocalRtc":{"packets":100},"egress":{"payloadBytes":40000}},"workers":[{"packetLoopDelayMs":4}]}
"#,
    );
    let report = render_runs(
        vec![run(ScenarioSpec::smoke(1, 50)?, None, 0)?.with_samples(samples)],
        None,
    )?;

    assert!(report.contains("| smoke-1r-50p | 3 | 2 | 2,000 ms | yes | yes |"));
    assert!(report.contains("SFU CPU timeline, common peak scale 100.000%"));
    assert!(report.contains("smoke-1r-50p +@"));
    assert!(report.contains(
        "SFU CPU average\n```text\nsmoke-1r-50p [################################] 75.000%"
    ));
    assert!(report.contains("SFU delivery efficiency"));
    assert!(report.contains("166 deliveries/CPU-s"));
    assert!(report.contains("50 packets/s"));
    assert!(report.contains("160.0 kbit/s"));
    assert!(report.contains("7 ms"));
    assert!(report.contains("3.0 MiB"));
    assert!(report.contains("## Telemetry issues"));
    assert!(report.contains("malformed telemetry record"));
    assert!(report.contains("scrape failed"));
    Ok(())
}

#[test]
fn telemetry_does_not_infer_cpu_percent_from_ticks() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"clockTicksPerSecond":100,"server":{"cpuTicks":10,"rssBytes":1048576,"startTimeTicks":5}}
{"elapsedMs":1000,"clockTicksPerSecond":100,"server":{"cpuTicks":30,"rssBytes":3145728,"startTimeTicks":5}}
"#,
    );
    let report = render_runs(
        vec![run(ScenarioSpec::smoke(1, 50)?, None, 0)?.with_samples(samples)],
        None,
    )?;

    assert!(
        report.contains(
            "SFU CPU average\n```text\nsmoke-1r-50p [................................] n/a"
        )
    );
    assert!(report.contains(
        "SFU RSS peak\n```text\nsmoke-1r-50p [################################] 3.0 MiB"
    ));
    Ok(())
}

#[test]
fn telemetry_cpu_average_is_weighted_by_sample_interval() {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"server":{"rssBytes":1}}
{"elapsedMs":1000,"serverCpuPercentMilli":10000,"rtcCpuPercentMilli":5000}
{"elapsedMs":4000,"serverCpuPercentMilli":20000,"rtcCpuPercentMilli":9000}
"#,
    );
    let summary = TelemetrySummary::from_samples(Some(&samples), 0);

    assert_eq!(summary.server_cpu_percent_milli, Some(17_500));
    assert_eq!(summary.rtc_cpu_percent_milli, Some(8_000));
}

#[test]
fn equal_sort_prefixes_still_render_deterministically() -> anyhow::Result<()> {
    let mut low_lag = run(ScenarioSpec::smoke(1, 50)?, None, 0)?;
    let mut high_lag = low_lag.clone();
    low_lag.result.max_send_lag_ms = 1;
    high_lag.result.max_send_lag_ms = 9;

    let forward = render_runs(vec![low_lag.clone(), high_lag.clone()], None)?;
    let reverse = render_runs(vec![high_lag, low_lag], None)?;

    assert_eq!(forward, reverse);
    Ok(())
}

#[test]
fn rate_and_rss_formatting_do_not_overflow() -> anyhow::Result<()> {
    let mut maximum = run(ScenarioSpec::smoke(1, 1)?, None, 0)?;
    maximum.result.delivered_packets = u64::MAX;
    maximum.result.elapsed_ms = 1;

    assert_eq!(delivery_rate(&maximum.result), u64::MAX);
    assert_eq!(format_mebibytes(u64::MAX), "17592186044415.9 MiB");
    Ok(())
}

#[test]
fn github_summary_accepts_exactly_one_mebibyte() {
    let exact = "x".repeat(GITHUB_SUMMARY_LIMIT_BYTES);
    let oversized = "x".repeat(GITHUB_SUMMARY_LIMIT_BYTES + 1);

    assert!(ensure_summary_size(&exact).is_ok());
    assert!(ensure_summary_size(&oversized).is_err());
}

#[test]
fn graph_scale_handles_maximum_counters() {
    assert_eq!(bar(u64::MAX, u64::MAX, '#'), "#".repeat(32));
    assert_eq!(bar(0, u64::MAX, '#'), ".".repeat(32));
}

#[test]
fn table_values_disable_active_markdown() {
    assert_eq!(
        escape_table("a\\|b\n[link](url)! *_`~<c>"),
        "a&#92;&#124;b &#91;link&#93;&#40;url&#41;&#33; &#42;&#95;&#96;&#126;&lt;c&gt;"
    );
}

#[test]
fn artifact_link_accepts_only_one_github_actions_artifact() {
    assert!(
        validate_artifact_url(Some(
            "https://github.com/example/repo/actions/runs/1/artifacts/2"
        ))
        .is_ok()
    );
    assert!(validate_artifact_url(Some("https://example.com/artifact")).is_err());
    assert!(
        validate_artifact_url(Some(
            "https://github.com/example/repo/actions/runs/1/artifacts/2?q=unsafe"
        ))
        .is_err()
    );
    assert!(
        validate_artifact_url(Some(
            "https://github.com/example/repo/actions/runs/1/artifacts/2)](https://example.com"
        ))
        .is_err()
    );
    assert!(validate_artifact_url(Some("https://github.com/example/repo\nunsafe")).is_err());
}

fn run(
    spec: ScenarioSpec,
    delivered_packets: Option<u64>,
    missing_packets: u64,
) -> anyhow::Result<RunData> {
    let plan = spec.plan()?;
    let delivered_packets = delivered_packets.unwrap_or(plan.expected_deliveries);
    let result = serde_json::from_value::<ScenarioResult>(json!({
        "schemaVersion": 2,
        "profile": spec.profile(),
        "oSfuRevision": "deadbeef",
        "scenario": spec,
        "serverPolicy": {
            "mediaWorkers": 1,
            "roomSize": spec.peers_per_room(),
            "maxActiveAudioSpeakers": spec.active_audio_speakers(),
            "maxVideoDownloadsPerReceiver": 10,
            "maxBitrateInBps": 8_000_000,
            "maxBitrateOutBps": 10_000_000
        },
        "plan": plan,
        "offeredPackets": plan.offered_packets,
        "offeredPayloadBytes": plan.offered_payload_bytes,
        "deliveredPackets": delivered_packets,
        "deliveredPayloadBytes": if delivered_packets == plan.expected_deliveries {
            plan.expected_delivery_payload_bytes
        } else {
            0
        },
        "correctness": {
            "missingPackets": missing_packets,
            "duplicatePackets": 0,
            "outOfOrderPackets": 0,
            "unexpectedPackets": 0,
            "payloadMismatches": 0
        },
        "elapsedMs": 1_000,
        "maxSendLagMs": 0
    }))?;
    Ok(RunData {
        source: "fixture".to_owned(),
        result,
        samples: None,
    })
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

fn render_runs(runs: Vec<RunData>, artifact_url: Option<&str>) -> anyhow::Result<String> {
    render_report(runs, Vec::new(), artifact_url)
}
