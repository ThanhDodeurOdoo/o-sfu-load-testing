use super::{
    ExpectedStream, MediaTurn, PacketLedger, PeerMedia, expected_audio_streams,
    expected_video_streams, featured_source, source_id,
};
use crate::{
    client::media::{AudioSource, PacketPhase, VideoLayer, VideoSource},
    video_packets_per_layer,
};

#[test]
fn ledger_accepts_cross_source_interleaving() -> anyhow::Result<()> {
    let mut first = AudioSource::new(0);
    let mut second = AudioSource::new(1);
    let mut ledger = PacketLedger::new(vec![
        ExpectedStream::audio(0, 2)?,
        ExpectedStream::audio(1, 2)?,
    ]);
    for payload in [
        first.next_packet(PacketPhase::Measured, 0).payload,
        second.next_packet(PacketPhase::Measured, 0).payload,
        second.next_packet(PacketPhase::Measured, 1).payload,
        first.next_packet(PacketPhase::Measured, 1).payload,
    ] {
        ledger.observe(&payload);
    }

    assert!(ledger.is_complete());
    assert_eq!(ledger.finish().discrepancy_count(), 0);
    Ok(())
}

#[test]
fn ledger_reports_duplicate_and_missing_packets() -> anyhow::Result<()> {
    let mut source = AudioSource::new(0);
    let first = source.next_packet(PacketPhase::Measured, 0).payload;
    let mut ledger = PacketLedger::new(vec![ExpectedStream::audio(0, 2)?]);
    ledger.observe(&first);
    ledger.observe(&first);
    let correctness = ledger.finish();

    assert_eq!(correctness.duplicate_packets, 1);
    assert_eq!(correctness.missing_packets, 1);
    Ok(())
}

#[test]
fn featured_video_source_never_selects_self() {
    assert_eq!(featured_source(0, 1), None);
    assert_eq!(featured_source(0, 3), Some(1));
    assert_eq!(featured_source(1, 3), Some(2));
    assert_eq!(featured_source(4, 3), Some(1));
}

#[test]
fn source_identity_is_unique_across_rooms() -> anyhow::Result<()> {
    let first_room = source_id(0, 2, 0)?;
    let second_room = source_id(1, 2, 0)?;

    assert_ne!(first_room, second_room);
    Ok(())
}

#[test]
fn mixed_media_turns_follow_one_interleaved_timeline() {
    let mut media = PeerMedia::mixed(Some(AudioSource::new(0)), 3, Some(VideoSource::new(0)), 2);

    assert_eq!(media.next_turn(0, 0), Some(MediaTurn::Audio));
    if let Some(source) = media.audio.as_mut() {
        let _packet = source.next_packet(PacketPhase::Measured, 0);
    }
    assert_eq!(media.next_turn(1, 0), Some(MediaTurn::Video));
    if let Some(source) = media.video.as_mut() {
        let _packets = source.next_frame(PacketPhase::Measured, 0);
    }
    assert_eq!(media.next_turn(1, 1), Some(MediaTurn::Audio));
    if let Some(source) = media.audio.as_mut() {
        let _packet = source.next_packet(PacketPhase::Measured, 1);
    }
    assert_eq!(media.next_turn(2, 1), Some(MediaTurn::Audio));
    if let Some(source) = media.audio.as_mut() {
        let _packet = source.next_packet(PacketPhase::Measured, 2);
    }
    assert_eq!(media.next_turn(3, 1), Some(MediaTurn::Video));
    if let Some(source) = media.video.as_mut() {
        let _packets = source.next_frame(PacketPhase::Measured, 1);
    }
    assert_eq!(media.next_turn(3, 2), None);
}

#[test]
fn mixed_media_turns_follow_staggered_source_deadlines() {
    let media = PeerMedia::mixed(
        Some(AudioSource::staggered(9, 9, 10)),
        1,
        Some(VideoSource::staggered(0, 0, 9)),
        1,
    );

    assert_eq!(media.next_turn(0, 0), Some(MediaTurn::Video));
    assert_eq!(media.next_turn(0, 1), Some(MediaTurn::Audio));
}

#[test]
fn mixed_expectations_exclude_self_from_both_media_kinds() -> anyhow::Result<()> {
    let (low_packets, high_packets) = video_packets_per_layer(1)?;

    let dual_publisher = expected_audio_streams(0, 20, 5, 0, 50)?.len()
        + expected_video_streams(0, 20, 4, 0, low_packets, high_packets)?.len();
    let audio_publisher = expected_audio_streams(0, 20, 5, 4, 50)?.len()
        + expected_video_streams(0, 20, 4, 4, low_packets, high_packets)?.len();
    let receiver = expected_audio_streams(0, 20, 5, 19, 50)?.len()
        + expected_video_streams(0, 20, 4, 19, low_packets, high_packets)?.len();

    assert_eq!(dual_publisher, 7);
    assert_eq!(audio_publisher, 8);
    assert_eq!(receiver, 9);
    Ok(())
}

#[test]
fn ledger_separates_audio_and_video_from_one_source() -> anyhow::Result<()> {
    let audio = AudioSource::new(7)
        .next_packet(PacketPhase::Measured, 0)
        .payload;
    let video = VideoSource::new(7)
        .next_frame(PacketPhase::Measured, 1)
        .into_iter()
        .find(|packet| packet.layer == VideoLayer::Low)
        .ok_or_else(|| anyhow::anyhow!("low video packet is missing"))?
        .payload;
    let mut ledger = PacketLedger::new(vec![
        ExpectedStream::audio(7, 1)?,
        ExpectedStream::video(7, VideoLayer::Low, 1)?,
    ]);

    ledger.observe(&audio);
    ledger.observe(&video);

    assert!(ledger.is_complete());
    assert_eq!(ledger.finish().discrepancy_count(), 0);
    Ok(())
}
