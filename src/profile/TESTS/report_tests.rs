use std::{collections::BTreeMap, env, fs, path::PathBuf, process};

use serde_json::json;

use super::{
    CapacityProcessStatus, FoldedProfile, display_stack, format_percent,
    informative_inclusive_frame, load_profile_run, ranked, render_ranked,
};

#[test]
fn folded_profile_aggregates_self_inclusive_thread_and_kernel_samples() -> anyhow::Result<()> {
    let profile = FoldedProfile::parse(
        "rtc-loop/1;main;route;send 60\nrtc-loop/1;main;route;send 10\nrtc-loop/2;main;[kernel.kallsyms <ffff>]_[k] 20\nrtc-loop/2;main;[o-sfu-load-server <7f00>] 10\n",
    )?;

    assert_eq!(profile.total_samples, 100);
    assert_eq!(profile.self_samples.get("send"), Some(&70));
    assert_eq!(profile.inclusive_samples.get("route"), Some(&70));
    assert_eq!(profile.thread_samples.get("rtc-loop/1"), Some(&70));
    assert_eq!(profile.kernel_samples, 20);
    assert_eq!(profile.unresolved_leaf_samples, 30);
    assert_eq!(profile.unresolved_stack_samples, 30);
    assert_eq!(profile.stack_samples.len(), 3);
    Ok(())
}

#[test]
fn unresolved_roots_do_not_hide_resolved_leaf_cost() -> anyhow::Result<()> {
    let profile = FoldedProfile::parse("rtc-loop/1;[libc.so.6 <7f00>];send 10\n")?;

    assert_eq!(profile.unresolved_leaf_samples, 0);
    assert_eq!(profile.unresolved_stack_samples, 10);
    Ok(())
}

#[test]
fn inclusive_samples_count_recursive_frames_once_per_stack() -> anyhow::Result<()> {
    let profile = FoldedProfile::parse("rtc-loop/1;poll;poll;send 40\n")?;

    assert_eq!(profile.inclusive_samples.get("poll"), Some(&40));
    assert_eq!(profile.inclusive_samples.get("send"), Some(&40));
    Ok(())
}

#[test]
fn malformed_folded_lines_remain_visible_without_losing_valid_samples() -> anyhow::Result<()> {
    let profile = FoldedProfile::parse(
        "rtc-loop/1;main;route 60\nmissing-count\nrtc-loop/1;main;send zero\n",
    )?;

    assert_eq!(profile.total_samples, 60);
    assert_eq!(profile.malformed_lines, 2);
    assert_eq!(profile.diagnostics.len(), 2);
    Ok(())
}

#[test]
fn ranked_samples_use_symbol_order_for_equal_counts() {
    let values = BTreeMap::from([
        ("zeta".to_owned(), 10),
        ("alpha".to_owned(), 10),
        ("hot".to_owned(), 20),
    ]);
    let labels = ranked(&values, 3)
        .into_iter()
        .map(|(label, _count)| label.as_str())
        .collect::<Vec<_>>();

    assert_eq!(labels, ["hot", "alpha", "zeta"]);
    assert_eq!(format_percent(1, 3), "33.33%");
}

#[test]
fn partition_breakdown_retains_the_omitted_share() -> anyhow::Result<()> {
    let values = (0..16)
        .map(|index| (format!("symbol-{index}"), 1))
        .collect::<BTreeMap<_, _>>();
    let mut output = String::new();

    render_ranked(&mut output, "Symbols", "Symbol", &values, 16, 15, true)?;

    assert!(output.contains("| Other | Remaining entries | 1 | 6.25% |"));
    Ok(())
}

#[test]
fn displayed_stacks_keep_the_thread_and_leaf_side() {
    let (thread, stack) = display_stack(
        "rtc-loop/1;very-long-runtime-root;packet-loop;source-policy;hot-leaf",
        40,
    );

    assert_eq!(thread, "rtc-loop/1");
    assert!(stack.starts_with("... -> "));
    assert!(stack.ends_with("source-policy -> hot-leaf"));
    assert!(!stack.contains("very-long-runtime-root"));
}

#[test]
fn inclusive_summary_omits_uninformative_roots() {
    assert!(!informative_inclusive_frame("[libc.so.6 <7f00>]"));
    assert!(!informative_inclusive_frame(
        "<std::sys::thread::unix::Thread>::new::thread_start"
    ));
    assert!(informative_inclusive_frame(
        "o_sfu_core::packet_loop::PacketLoopTurn::pump"
    ));
}

#[test]
fn failed_profile_run_keeps_scenario_context() -> anyhow::Result<()> {
    let directory = test_directory("failed-run");
    fs::create_dir_all(&directory)?;
    let scenario = crate::ScenarioSpec::mixed_conference(1, 100, 10, 9, 60)?;
    fs::write(
        directory.join("scenario.json"),
        serde_json::to_vec(&scenario)?,
    )?;
    fs::write(directory.join("capacity.status"), "FAIL\n")?;

    let (loaded, revision, exact, capacity_process) = load_profile_run(&directory)?;

    assert_eq!(loaded, scenario);
    assert_eq!(revision.as_deref(), Some(crate::O_SFU_REVISION));
    assert!(!exact);
    assert_eq!(capacity_process, Some(CapacityProcessStatus::Failed));
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn profile_summary_explains_overlapping_costs_and_raw_evidence() -> anyhow::Result<()> {
    let directory = test_directory("summary");
    fs::create_dir_all(&directory)?;
    let scenario = crate::ScenarioSpec::mixed_conference(1, 100, 10, 9, 60)?;
    let plan = scenario.plan()?;
    let result = json!({
        "schemaVersion": 4,
        "profile": scenario.profile(),
        "oSfuRevision": "1111111111111111111111111111111111111111",
        "scenario": scenario,
        "serverPolicy": {
            "mediaWorkers": 1,
            "roomSize": scenario.peers_per_room(),
            "maxPreAuthWebsocketSessionsPerOrigin": scenario.room_count() * scenario.peers_per_room(),
            "maxActiveAudioSpeakers": scenario.active_audio_speakers(),
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
        "elapsedMs": 1_000,
        "maxSendLagMs": 0
    });
    fs::write(directory.join("result.json"), serde_json::to_vec(&result)?)?;
    fs::write(directory.join("capacity.status"), "FAIL\n")?;
    fs::write(
        directory.join(super::CAPTURE_FILE),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "event": "cpu-clock",
            "frequencyHz": 99,
            "callGraph": "fp",
            "durationMs": 2_500
        }))?,
    )?;
    fs::write(
        directory.join(super::ENVIRONMENT_FILE),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "cpuModel": "Example CPU",
            "logicalCpus": 4,
            "kernel": "Linux 6.8.0 x86_64 GNU/Linux",
            "perfVersion": "perf version 6.8.0",
            "infernoVersion": "0.12.8",
            "rustcVersion": "rustc 1.95.0",
            "runnerImage": "ubuntu24 20260720.1",
            "perfEventMaxStack": "127"
        }))?,
    )?;
    fs::write(
        directory.join(super::FOLDED_FILE),
        "rtc-loop/1;main;route;send 60\nrtc-loop/2;main;sys_sendmsg_[k] 30\nrtc-loop/2;main;[unknown] 10\n",
    )?;
    fs::write(directory.join(super::FLAMEGRAPH_FILE), "<svg/>")?;
    for name in [
        super::PROFILE_READY_FILE,
        super::PERF_DATA_FILE,
        "perf-header.txt",
        "hotspots-self.txt",
        "hotspots-inclusive.txt",
        "threads.txt",
    ] {
        fs::write(directory.join(name), "present")?;
    }

    let artifact_url = "https://github.com/example/repo/actions/runs/1/artifacts/2";
    let flamegraph_url = "https://github.com/example/repo/releases/download/load-test-assets/o-sfu-flamegraph-1-1.png";
    let summary = super::render(&directory, Some(artifact_url), Some(flamegraph_url))?;

    assert!(summary.contains("| AVAILABLE | FAIL | PASS | mixed-conference-1x100-10a-9v-60s |"));
    assert!(summary.contains("| cpu-clock | fp | 99 Hz | 2.500 s | 100 | 10 (10.00%) |"));
    assert!(summary.contains("| CPU model | Example CPU |"));
    assert!(summary.contains("| Maximum stack depth | 127 |"));
    assert!(summary.contains("| Kernel | 30 | 30.00% |"));
    assert!(summary.contains("| Unresolved leaf | 10 | 10.00% |"));
    assert!(summary.contains("| Partially symbolized stack | 10 | 10.00% |"));
    assert!(summary.contains("Self cost attributes each sample to its leaf frame"));
    assert!(summary.contains("Inclusive cost counts a frame once"));
    assert!(summary.contains(&format!(
        "[![o-sfu CPU flamegraph]({flamegraph_url})]({artifact_url})"
    )));
    assert!(summary.contains("hotspots-self.txt"));

    fs::remove_dir_all(directory)?;
    Ok(())
}

fn test_directory(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("o-sfu-load-profile-{label}-{}-{id}", process::id()))
}
