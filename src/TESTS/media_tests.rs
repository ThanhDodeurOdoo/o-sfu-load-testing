use std::time::Duration;

use super::{
    AUDIO_FRAME_INTERVAL, AudioSource, MediaTimeline, OPUS_HYBRID_FULLBAND_20_MS_CONFIG,
    PacketPhase, PayloadKind, VIDEO_FRAME_INTERVAL, VideoLayer, VideoSource, inspect_payload,
};
use crate::AUDIO_PACKET_PAYLOAD_BYTES;

#[test]
fn audio_payload_identifies_equal_ordinals_from_distinct_sources() -> anyhow::Result<()> {
    let first = AudioSource::new(3).next_packet(PacketPhase::Measured, 0);
    let second = AudioSource::new(4).next_packet(PacketPhase::Measured, 0);
    let first_identity = inspect_payload(&first.payload)?.identity;
    let second_identity = inspect_payload(&second.payload)?.identity;

    assert_eq!(first_identity.kind, PayloadKind::Audio);
    assert_eq!(first_identity.source, 3);
    assert_eq!(second_identity.source, 4);
    assert_ne!(first.payload, second.payload);
    Ok(())
}

#[test]
fn audio_payload_models_fullband_speech() {
    let packet = AudioSource::new(0).next_packet(PacketPhase::Measured, 0);

    assert_eq!(packet.payload.len(), AUDIO_PACKET_PAYLOAD_BYTES);
    assert_eq!(
        packet.payload.first().map(|toc| toc >> 3),
        Some(OPUS_HYBRID_FULLBAND_20_MS_CONFIG)
    );
}

#[test]
fn warmup_and_measured_audio_have_distinct_identity() -> anyhow::Result<()> {
    let mut source = AudioSource::new(2);
    let warmup = source.next_packet(PacketPhase::Warmup, 0);
    let measured = source.next_packet(PacketPhase::Measured, 0);

    assert_ne!(
        inspect_payload(&warmup.payload)?.identity.phase,
        inspect_payload(&measured.payload)?.identity.phase
    );
    Ok(())
}

#[test]
fn video_source_emits_frame_bursts_for_both_layers() {
    let packets = VideoSource::new(7).next_frame(PacketPhase::Measured, 0);
    let low = packets
        .iter()
        .filter(|packet| packet.layer == VideoLayer::Low)
        .count();
    let high = packets
        .iter()
        .filter(|packet| packet.layer == VideoLayer::High)
        .count();

    assert_eq!(low, 2);
    assert_eq!(high, 20);
    assert!(packets.iter().all(|packet| {
        inspect_payload(&packet.payload).is_ok_and(|inspection| inspection.payload_matches)
    }));
}

#[test]
fn mixed_sources_stagger_first_packets_within_one_interval() {
    let first_audio = AudioSource::staggered(0, 0, 10)
        .next_packet(PacketPhase::Measured, 0)
        .emitted_at;
    let last_audio = AudioSource::staggered(9, 9, 10)
        .next_packet(PacketPhase::Measured, 0)
        .emitted_at;
    let first_video = VideoSource::staggered(0, 0, 9)
        .next_frame(PacketPhase::Measured, 0)
        .first()
        .map(|packet| packet.emitted_at);
    let last_video = VideoSource::staggered(8, 8, 9)
        .next_frame(PacketPhase::Measured, 0)
        .first()
        .map(|packet| packet.emitted_at);

    assert!(first_audio < last_audio);
    assert_eq!(last_audio, super::AUDIO_FRAME_INTERVAL);
    assert!(
        first_video
            .zip(last_video)
            .is_some_and(|(first, last)| { first < last && last <= super::VIDEO_FRAME_INTERVAL })
    );
}

#[test]
fn mixed_source_deadlines_fit_the_requested_minute() {
    let target = Duration::from_mins(1);
    for rank in 0..10 {
        let mut timeline = MediaTimeline::staggered(AUDIO_FRAME_INTERVAL, rank, 10);
        let mut final_deadline = Duration::ZERO;
        for _ in 0..3_000 {
            final_deadline = timeline.advance();
        }
        assert!(final_deadline <= target);
    }
    for rank in 0..9 {
        let mut timeline = MediaTimeline::staggered(VIDEO_FRAME_INTERVAL, rank, 9);
        let mut final_deadline = Duration::ZERO;
        for _ in 0..1_800 {
            final_deadline = timeline.advance();
        }
        assert!(final_deadline <= target);
    }
}

#[test]
fn payload_inspection_detects_body_corruption() -> anyhow::Result<()> {
    let mut packet = AudioSource::new(1).next_packet(PacketPhase::Measured, 0);
    let last = packet.payload.len().saturating_sub(1);
    if let Some(byte) = packet.payload.get_mut(last) {
        *byte ^= 1;
    }

    assert!(!inspect_payload(&packet.payload)?.payload_matches);
    Ok(())
}
