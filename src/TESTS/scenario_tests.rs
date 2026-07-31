use super::{ExpectedStream, PacketLedger, featured_source, source_id};
use crate::client::media::{AudioSource, PacketPhase};

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
