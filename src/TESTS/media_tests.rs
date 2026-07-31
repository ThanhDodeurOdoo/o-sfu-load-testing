use super::{AudioSource, PacketPhase, PayloadKind, VideoLayer, VideoSource, inspect_payload};

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
fn payload_inspection_detects_body_corruption() -> anyhow::Result<()> {
    let mut packet = AudioSource::new(1).next_packet(PacketPhase::Measured, 0);
    let last = packet.payload.len().saturating_sub(1);
    if let Some(byte) = packet.payload.get_mut(last) {
        *byte ^= 1;
    }

    assert!(!inspect_payload(&packet.payload)?.payload_matches);
    Ok(())
}
