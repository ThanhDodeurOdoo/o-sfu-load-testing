use serde_json::json;

use super::{CorrectnessSummary, RunObservation, ScenarioResult, ScenarioSpec, ServerPolicy};

#[test]
fn smoke_plan_preserves_foundation_cardinality() -> anyhow::Result<()> {
    let spec = ScenarioSpec::smoke(2, 50)?;
    let plan = spec.plan()?;

    assert_eq!(plan.offered_packets, 50);
    assert_eq!(plan.expected_deliveries, 100);
    Ok(())
}

#[test]
fn audio_mesh_plan_counts_every_source_route() -> anyhow::Result<()> {
    let spec = ScenarioSpec::audio_mesh(2, 4, 1)?;
    let plan = spec.plan()?;

    assert_eq!(plan.streams, 8);
    assert_eq!(plan.routes, 24);
    assert_eq!(plan.offered_packets, 400);
    assert_eq!(plan.expected_deliveries, 1_200);
    Ok(())
}

#[test]
fn video_plan_counts_featured_and_thumbnail_layers() -> anyhow::Result<()> {
    let spec = ScenarioSpec::video_gallery(1, 6, 3, 2)?;
    let plan = spec.plan()?;

    assert_eq!(plan.streams, 6);
    assert_eq!(plan.routes, 15);
    assert_eq!(plan.offered_packets, 2_721);
    assert_eq!(plan.expected_deliveries, 5_625);
    Ok(())
}

#[test]
fn mixed_conference_plan_counts_overlapping_publishers() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 100, 10, 9, 60)?;
    let plan = spec.plan()?;

    assert_eq!(plan.streams, 28);
    assert_eq!(plan.routes, 1_881);
    assert_eq!(plan.offered_packets, 274_890);
    assert_eq!(plan.offered_payload_bytes, 263_544_000);
    assert_eq!(plan.expected_deliveries, 6_955_530);
    assert_eq!(plan.expected_delivery_payload_bytes, 3_897_918_000);
    assert_eq!(spec.audio_publishers_per_room(), 10);
    assert_eq!(spec.video_publishers_per_room(), 9);
    assert_eq!(
        ServerPolicy::for_scenario(spec).max_active_audio_speakers,
        10
    );
    Ok(())
}

#[test]
fn mixed_conference_serializes_camel_case_coordinates() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 100, 10, 9, 60)?;

    assert_eq!(
        serde_json::to_value(spec)?,
        json!({
            "type": "mixed-conference",
            "rooms": 1,
            "peers": 100,
            "audioPublishers": 10,
            "videoPublishers": 9,
            "seconds": 60,
        })
    );
    Ok(())
}

#[test]
fn result_rejects_any_exact_delivery_discrepancy() -> anyhow::Result<()> {
    let spec = ScenarioSpec::audio_mesh(1, 2, 1)?;
    let plan = spec.plan()?;
    let observation = RunObservation {
        offered_packets: plan.offered_packets,
        offered_payload_bytes: plan.offered_payload_bytes,
        delivered_packets: plan.expected_deliveries,
        delivered_payload_bytes: plan.expected_delivery_payload_bytes,
        elapsed_ms: 1_000,
        ..RunObservation::default()
    };
    let mut result = ScenarioResult::completed(spec, observation)?;
    result.validate(spec)?;

    result.correctness = CorrectnessSummary {
        duplicate_packets: 1,
        ..CorrectnessSummary::default()
    };
    assert!(result.validate(spec).is_err());
    Ok(())
}

#[test]
fn scenarios_reject_empty_or_unbounded_work() {
    assert!(ScenarioSpec::smoke(1, 0).is_err());
    assert!(ScenarioSpec::audio_mesh(0, 2, 1).is_err());
    assert!(ScenarioSpec::audio_mesh(65, 2, 1).is_err());
    assert!(ScenarioSpec::audio_mesh(1, 101, 1).is_err());
    assert!(ScenarioSpec::video_gallery(1, 3, 11, 1).is_err());
    assert!(ScenarioSpec::mixed_conference(1, 100, 8, 9, 60).is_err());
    assert!(ScenarioSpec::mixed_conference(1, 100, 10, 11, 60).is_err());
}

#[test]
fn malformed_scenario_plans_return_errors() {
    assert!(
        ScenarioSpec::AudioMesh {
            rooms: 1,
            peers: 0,
            seconds: 1,
        }
        .plan()
        .is_err()
    );
    assert!(
        ScenarioSpec::VideoGallery {
            rooms: 1,
            peers: 0,
            publishers: 1,
            seconds: 1,
        }
        .plan()
        .is_err()
    );
    assert!(
        ScenarioSpec::MixedConference {
            rooms: 1,
            peers: 0,
            audio_publishers: 1,
            video_publishers: 1,
            seconds: 1,
        }
        .plan()
        .is_err()
    );
}

#[test]
fn server_admission_matches_concurrent_peer_count() -> anyhow::Result<()> {
    let spec = ScenarioSpec::audio_mesh(4, 16, 1)?;

    assert_eq!(
        ServerPolicy::for_scenario(spec).max_pre_auth_websocket_sessions_per_origin,
        64
    );
    Ok(())
}
