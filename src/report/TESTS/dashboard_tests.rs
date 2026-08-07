use std::{
    env, fs, process, slice,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context as _;

use super::{
    DashboardAsset, DashboardConfig, DashboardData, DashboardLimits, RevisionRole, Unit,
    contiguous_segments, counter_series, direct_series, matching_pair, moving_average,
    render_assets, render_svg, unique_scenario, validate_config,
};
use crate::{
    RunObservation, ScenarioResult, ScenarioSpec,
    report::{RunData, parse_samples},
};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn dashboard_renders_rich_telemetry_panels() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"scrapeDurationMs":4,"server":{"rssBytes":1048576},"rtc":{"rssBytes":2097152},"traffic":{"ingress":{"packets":0,"payloadBytes":0},"egress":{"packets":0,"payloadBytes":0},"forwardedLocalRtc":{"packets":0,"payloadBytes":0}},"workers":[{"mediaWorkerId":0,"egressBitrateBps":0,"packetLoopDelayMs":2,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0}]}
{"elapsedMs":0,"status":"phase","phase":"setup"}
{"elapsedMs":500,"status":"phase","phase":"warmup"}
{"elapsedMs":1000,"status":"phase","phase":"measured"}
{"elapsedMs":1000,"scrapeDurationMs":5,"serverCpuPercentMilli":25000,"rtcCpuPercentMilli":50000,"server":{"rssBytes":3145728},"rtc":{"rssBytes":4194304},"traffic":{"ingress":{"packets":100,"payloadBytes":8000},"egress":{"packets":500,"payloadBytes":40000},"forwardedLocalRtc":{"packets":500,"payloadBytes":40000}},"workers":[{"mediaWorkerId":0,"egressBitrateBps":320000,"packetLoopDelayMs":7,"commandBacklogDepth":2,"relayMailboxDepth":3,"workerPressureScore":4}]}
{"elapsedMs":2000,"scrapeDurationMs":6,"finalSample":true,"serverCpuPercentMilli":50000,"rtcCpuPercentMilli":75000,"server":{"rssBytes":5242880},"rtc":{"rssBytes":6291456},"traffic":{"ingress":{"packets":200,"payloadBytes":16000},"egress":{"packets":1000,"payloadBytes":80000},"forwardedLocalRtc":{"packets":1000,"payloadBytes":80000}},"workers":[{"mediaWorkerId":0,"egressBitrateBps":640000,"packetLoopDelayMs":null,"commandBacklogDepth":4,"relayMailboxDepth":5,"workerPressureScore":6}]}
{"elapsedMs":2500,"status":"phase","phase":"drain"}
"#,
    );
    let directory = env::temp_dir().join(format!(
        "o-sfu-load-dashboard-{}-{}",
        process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory)?;
    let output = directory.join("dashboard.svg");

    render_svg(
        &DashboardData::new(&samples, None, 1)?,
        "video-gallery-1x12-4p-30s",
        &output,
    )?;

    let svg = fs::read_to_string(output)?;
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("video-gallery-1x12-4p-30s"));
    assert!(svg.contains("RTP packets / second"));
    assert!(svg.contains("RTP payload Mbit / second"));
    assert!(svg.contains("Packet-loop delay"));
    assert!(svg.contains("Queue depth"));
    assert!(svg.contains("measured"));
    let drain = svg.find("drain").context("missing drain phase label")?;
    let label = svg[..drain]
        .rfind("<text")
        .map(|start| &svg[start..drain])
        .context("missing drain text element")?;
    assert!(label.contains("text-anchor=\"end\""));
    Ok(())
}

#[test]
fn counter_rates_use_real_intervals_and_leave_resets_as_gaps() {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"traffic":{"ingress":{"packets":0}}}
{"elapsedMs":1000,"traffic":{"ingress":{"packets":100}}}
{"elapsedMs":3000,"traffic":{"ingress":{"packets":300}}}
{"elapsedMs":4000,"traffic":{"ingress":{"packets":10}}}
{"elapsedMs":5000,"traffic":{"ingress":{"packets":60}}}
"#,
    );

    let rates = counter_series(
        "Ingress",
        super::COLOR_BLUE,
        &samples.samples,
        |sample| sample.ingress_packets,
        1_000,
    );

    assert_eq!(
        rates.values,
        [
            None,
            Some((1_000, 100)),
            Some((3_000, 100)),
            None,
            Some((5_000, 50)),
        ]
    );
    assert_eq!(
        moving_average(&rates.values, 2),
        [
            None,
            Some((1_000, 100)),
            Some((3_000, 100)),
            None,
            Some((5_000, 50)),
        ]
    );
}

#[test]
fn telemetry_errors_split_direct_and_counter_series() {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"serverCpuPercentMilli":1000,"traffic":{"ingress":{"packets":0}}}
{"elapsedMs":1000,"serverCpuPercentMilli":2000,"traffic":{"ingress":{"packets":100}}}
{"elapsedMs":2000,"scrapeDurationMs":750,"status":"error","message":"scrape timed out"}
{"elapsedMs":3000,"serverCpuPercentMilli":3000,"traffic":{"ingress":{"packets":300}}}
{"elapsedMs":4000,"serverCpuPercentMilli":4000,"traffic":{"ingress":{"packets":400}}}
"#,
    );
    let cpu = direct_series(
        "SFU",
        super::COLOR_BLUE,
        &samples.samples,
        |sample| sample.server_cpu_percent_milli,
        true,
    );
    let packets = counter_series(
        "Ingress",
        super::COLOR_BLUE,
        &samples.samples,
        |sample| sample.ingress_packets,
        1_000,
    );

    assert_eq!(samples.unavailable, 1);
    assert_eq!(contiguous_segments(&cpu.values).len(), 2);
    assert_eq!(
        packets.values,
        [None, Some((1_000, 100)), None, None, Some((4_000, 100)),]
    );
}

#[test]
fn dashboard_rejects_workers_outside_the_result_policy() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"workers":[{"mediaWorkerId":1,"egressBitrateBps":1,"packetLoopDelayMs":1,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0}]}
"#,
    );
    let run = scenario_run(ScenarioSpec::smoke(1, 1)?)?;

    let error = DashboardData::from_run(&run, &samples)
        .err()
        .context("worker outside policy unexpectedly rendered")?;

    assert!(error.to_string().contains("worker 1"));
    assert!(error.to_string().contains("1 media worker"));
    Ok(())
}

#[test]
fn dashboard_rejects_worker_cardinality_above_the_result_policy() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"workers":[{"mediaWorkerId":0,"egressBitrateBps":1,"packetLoopDelayMs":1,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0},{"mediaWorkerId":1,"egressBitrateBps":1,"packetLoopDelayMs":1,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0}]}
"#,
    );
    let run = scenario_run(ScenarioSpec::smoke(1, 1)?)?;

    let error = DashboardData::from_run(&run, &samples)
        .err()
        .context("excess workers unexpectedly rendered")?;

    assert!(error.to_string().contains("contains 2 workers"));
    assert!(error.to_string().contains("1 media worker"));
    Ok(())
}

#[test]
fn dashboard_rejects_worker_policies_above_the_parser_bound() -> anyhow::Result<()> {
    let samples = parse_samples(r#"{"elapsedMs":0,"scrapeDurationMs":1}"#);
    let mut run = scenario_run(ScenarioSpec::smoke(1, 1)?)?;
    run.result.server_policy.media_workers = 65;

    let error = DashboardData::from_run(&run, &samples)
        .err()
        .context("unbounded worker policy unexpectedly rendered")?;

    assert!(error.to_string().contains("at most 64 media workers"));
    Ok(())
}

#[test]
fn any_unresponsive_worker_gaps_the_aggregate_delay() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"workers":[{"mediaWorkerId":0,"egressBitrateBps":1,"packetLoopDelayMs":7,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0},{"mediaWorkerId":1,"egressBitrateBps":1,"packetLoopDelayMs":null,"commandBacklogDepth":0,"relayMailboxDepth":0,"workerPressureScore":0}]}
"#,
    );
    let dashboard = DashboardData::new(&samples, None, 2)?;
    let delay = dashboard
        .panels
        .iter()
        .find(|panel| panel.title == "Packet-loop delay")
        .and_then(|panel| panel.series.first())
        .context("missing packet-loop delay series")?;

    assert!(
        samples
            .samples
            .first()
            .is_some_and(|sample| sample.packet_loop_unresponsive)
    );
    assert_eq!(delay.values, [None]);
    Ok(())
}

#[test]
fn expected_fanout_uses_the_planned_delivery_ratio() -> anyhow::Result<()> {
    let samples = parse_samples(r#"{"elapsedMs":0,"scrapeDurationMs":1}"#);
    let run = scenario_run(ScenarioSpec::smoke(2, 50)?)?;
    let dashboard = DashboardData::from_run(&run, &samples)?;
    let expected = dashboard
        .panels
        .iter()
        .find(|panel| panel.title == "Local fanout multiplier")
        .and_then(|panel| panel.series.iter().find(|series| series.name == "Expected"))
        .context("missing expected fanout series")?;

    assert_eq!(expected.values, [Some((0, 2_000))]);
    Ok(())
}

#[test]
fn unavailable_scrapes_render_as_separate_points() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"scrapeDurationMs":4}
{"elapsedMs":1000,"scrapeDurationMs":750,"status":"error","message":"scrape timed out"}
"#,
    );
    let dashboard = DashboardData::new(&samples, None, 1)?;
    let scrape = dashboard
        .panels
        .iter()
        .find(|panel| panel.title == "Telemetry scrape duration")
        .context("missing telemetry scrape panel")?;
    let successful = scrape
        .series
        .iter()
        .find(|series| series.name == "Successful scrape")
        .context("missing successful scrape series")?;
    let unavailable = scrape
        .series
        .iter()
        .find(|series| series.name == "Unavailable scrape")
        .context("missing unavailable scrape series")?;

    assert_eq!(successful.values, [Some((0, 4)), None]);
    assert_eq!(unavailable.values, [None, Some((1_000, 750))]);
    assert!(unavailable.points_only);
    Ok(())
}

#[test]
fn low_bitrate_axes_keep_meaningful_precision() {
    assert_eq!(Unit::MegabitsPerSecond.format(64_000), "0.064");
    assert_eq!(Unit::MegabitsPerSecond.format(4_000_000), "4.0");
    assert_eq!(Unit::Cpu.format(2_000), "2.00");
}

#[test]
fn phase_boundaries_gap_interval_derived_values() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"serverCpuPercentMilli":1000}
{"elapsedMs":0,"status":"phase","phase":"setup"}
{"elapsedMs":500,"status":"phase","phase":"warmup"}
{"elapsedMs":1000,"serverCpuPercentMilli":9000}
{"elapsedMs":1500,"status":"phase","phase":"measured"}
{"elapsedMs":2000,"serverCpuPercentMilli":8000}
{"elapsedMs":2500,"status":"phase","phase":"drain"}
{"elapsedMs":3000,"serverCpuPercentMilli":7000}
{"elapsedMs":4000,"serverCpuPercentMilli":5000}
"#,
    );

    let dashboard = DashboardData::new(&samples, None, 1)?;
    let cpu = dashboard
        .panels
        .first()
        .and_then(|panel| panel.series.first());

    assert_eq!(
        cpu.map(|series| series.values.as_slice()),
        Some([Some((0, 1_000)), None, None, None, Some((4_000, 5_000)),].as_slice())
    );
    assert_eq!(
        cpu.and_then(|series| series.raw_values.as_deref()),
        Some(
            [
                Some((0, 1_000)),
                Some((1_000, 9_000)),
                Some((2_000, 8_000)),
                Some((3_000, 7_000)),
                Some((4_000, 5_000)),
            ]
            .as_slice()
        )
    );
    assert_eq!(dashboard.limits().panel_maxima.first(), Some(&9_900));
    Ok(())
}

#[test]
fn failed_scrape_does_not_hide_a_cpu_phase_crossing() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0,"serverCpuPercentMilli":1000}
{"elapsedMs":0,"status":"phase","phase":"setup"}
{"elapsedMs":500,"status":"phase","phase":"warmup"}
{"elapsedMs":750,"status":"phase","phase":"measured"}
{"elapsedMs":1000,"scrapeDurationMs":750,"status":"error","message":"scrape timed out"}
{"elapsedMs":2000,"serverCpuPercentMilli":8000}
{"elapsedMs":2500,"status":"phase","phase":"drain"}
{"elapsedMs":3000,"serverCpuPercentMilli":7000}
{"elapsedMs":4000,"serverCpuPercentMilli":5000}
"#,
    );
    let dashboard = DashboardData::new(&samples, None, 1)?;
    let cpu = dashboard
        .panels
        .first()
        .and_then(|panel| panel.series.first())
        .context("missing SFU CPU series")?;

    assert_eq!(
        cpu.interval_starts_ms.as_deref(),
        Some([None, None, Some(0), Some(2_000), Some(3_000)].as_slice())
    );
    assert_eq!(
        cpu.values,
        [Some((0, 1_000)), None, None, None, Some((4_000, 5_000)),]
    );
    assert_eq!(
        cpu.raw_values.as_deref(),
        Some(
            [
                Some((0, 1_000)),
                None,
                Some((2_000, 8_000)),
                Some((3_000, 7_000)),
                Some((4_000, 5_000)),
            ]
            .as_slice()
        )
    );
    Ok(())
}

#[test]
fn raw_only_series_retain_scale_and_legend() -> anyhow::Result<()> {
    let samples = parse_samples(
        r#"
{"elapsedMs":0}
{"elapsedMs":0,"status":"phase","phase":"setup"}
{"elapsedMs":500,"status":"phase","phase":"warmup"}
{"elapsedMs":1000,"serverCpuPercentMilli":25000}
{"elapsedMs":1500,"status":"phase","phase":"measured"}
{"elapsedMs":2000,"serverCpuPercentMilli":30000}
{"elapsedMs":2500,"status":"phase","phase":"drain"}
{"elapsedMs":3000,"serverCpuPercentMilli":35000}
"#,
    );
    let dashboard = DashboardData::new(&samples, None, 1)?;
    let directory = env::temp_dir().join(format!(
        "o-sfu-load-dashboard-{}-{}",
        process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory)?;
    let output = directory.join("raw-only.svg");

    assert_eq!(dashboard.limits().panel_maxima.first(), Some(&38_500));
    render_svg(&dashboard, "raw-only", &output)?;

    let svg = fs::read_to_string(output)?;
    assert!(svg.contains(">\nSFU\n</text>"));
    assert!(svg.contains("r=\"3\" opacity=\"0.25\" fill=\"#388BFD\""));
    assert!(!svg.contains("<polyline fill=\"none\" opacity=\"0.25\" stroke=\"#388BFD\""));
    Ok(())
}

#[test]
fn dashboard_sections_keep_planned_scenario_order() -> anyhow::Result<()> {
    let assets = [
        DashboardAsset {
            scenario: "smoke-1r-50p".to_owned(),
            role: RevisionRole::Single,
            revision: "a".repeat(40),
            file_name: "stem-single-smoke-1r-50p.svg".to_owned(),
        },
        DashboardAsset {
            scenario: "audio-mesh-1x8-30s".to_owned(),
            role: RevisionRole::Single,
            revision: "a".repeat(40),
            file_name: "stem-single-audio-mesh-1x8-30s.svg".to_owned(),
        },
    ];

    let markdown = render_assets(
        &assets,
        Some("https://github.com/example/repo/releases/download/load-test-assets"),
        None,
    )?;

    let smoke = markdown.find("<summary>smoke-1r-50p</summary>");
    let audio = markdown.find("<summary>audio-mesh-1x8-30s</summary>");
    assert!(matches!((smoke, audio), (Some(smoke), Some(audio)) if smoke < audio));
    assert!(markdown.contains("<details open>\n<summary>audio-mesh-1x8-30s</summary>"));
    Ok(())
}

#[test]
fn comparison_dashboards_share_elapsed_and_value_axes() -> anyhow::Result<()> {
    let baseline = DashboardData::new(
        &parse_samples(
            r#"
{"elapsedMs":0,"serverCpuPercentMilli":1000}
{"elapsedMs":1000,"serverCpuPercentMilli":2000}
"#,
        ),
        None,
        1,
    )?;
    let comparison = DashboardData::new(
        &parse_samples(
            r#"
{"elapsedMs":0,"serverCpuPercentMilli":3000}
{"elapsedMs":2000,"serverCpuPercentMilli":4000}
"#,
        ),
        None,
        1,
    )?;

    let limits = DashboardLimits::shared(&baseline, &comparison);

    assert_eq!(limits.elapsed_ms, 2_000);
    assert_eq!(limits.panel_maxima.first(), Some(&4_400));
    Ok(())
}

#[test]
fn dashboard_pairing_requires_unique_matching_workloads() -> anyhow::Result<()> {
    let scenario = ScenarioSpec::smoke(1, 50)?;
    let baseline = scenario_run(scenario)?;
    let comparison = scenario_run(scenario)?;

    assert!(unique_scenario(slice::from_ref(&baseline), scenario).is_some());
    assert!(
        matching_pair(
            slice::from_ref(&baseline),
            slice::from_ref(&comparison),
            scenario
        )
        .is_some()
    );
    assert!(unique_scenario(&[baseline.clone(), baseline], scenario).is_none());
    assert!(
        matching_pair(
            &[comparison.clone(), comparison.clone()],
            slice::from_ref(&comparison),
            scenario
        )
        .is_none()
    );

    let baseline = scenario_run(scenario)?;
    let mut mismatched = scenario_run(scenario)?;
    mismatched.result.profile = "different-profile".to_owned();
    assert!(matching_pair(&[baseline], &[mismatched], scenario).is_none());
    Ok(())
}

#[test]
fn dashboard_publication_accepts_only_the_visual_release() {
    let output = env::temp_dir();
    let valid = DashboardConfig {
        output_directory: &output,
        asset_stem: "o-sfu-telemetry-1-2",
        public_url_base: Some("https://github.com/example/repo/releases/download/load-test-assets"),
    };
    let unsafe_base = DashboardConfig {
        public_url_base: Some("https://example.com/load-test-assets"),
        ..valid
    };
    let unsafe_stem = DashboardConfig {
        asset_stem: "../telemetry",
        ..valid
    };

    assert!(validate_config(&valid).is_ok());
    assert!(validate_config(&unsafe_base).is_err());
    assert!(validate_config(&unsafe_stem).is_err());
}

fn scenario_run(scenario: ScenarioSpec) -> anyhow::Result<RunData> {
    Ok(RunData {
        source: "fixture".to_owned(),
        result: ScenarioResult::completed(scenario, RunObservation::default())?,
        samples: None,
    })
}
