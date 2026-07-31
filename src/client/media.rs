use std::time::Duration;

use o_sfu_rfc::rtp::opus;
use str0m::rtp::ExtensionValues;

const FRAME_INTERVAL: Duration = Duration::from_millis(20);
const TIMESTAMP_STEP: u32 = opus::RTP_CLOCK_RATE_HZ / 50;
const PACKET_PAYLOAD_LEN: usize = 160;
const FRAME_BODY_LEN: usize = PACKET_PAYLOAD_LEN - 1;
const PAYLOAD_SEED: u8 = 0x11;
const ONE_FRAME_TOC: u8 =
    (opus::toc_config::SILK_WIDEBAND_20_MS << 3) | opus::frame_count_code::ONE_FRAME;

pub struct AudioPacket {
    pub emitted_at: Duration,
    pub rtp_timestamp: u32,
    pub sequence_number: u16,
    pub extension_values: ExtensionValues,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub struct AudioSource {
    emitted_at: Duration,
    next_rtp_timestamp: u32,
    next_sequence_number: u16,
}

impl AudioSource {
    pub fn next_packet(&mut self) -> AudioPacket {
        self.emitted_at += FRAME_INTERVAL;
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
        let rtp_timestamp = self.next_rtp_timestamp;
        self.next_rtp_timestamp = self.next_rtp_timestamp.wrapping_add(TIMESTAMP_STEP);
        AudioPacket {
            emitted_at: self.emitted_at,
            rtp_timestamp,
            sequence_number,
            extension_values: ExtensionValues {
                audio_level: Some(-32),
                voice_activity: Some(true),
                ..ExtensionValues::default()
            },
            payload: opus_payload(sequence_number, rtp_timestamp),
        }
    }
}

fn opus_payload(sequence_number: u16, rtp_timestamp: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(PACKET_PAYLOAD_LEN);
    payload.push(ONE_FRAME_TOC);
    for byte in sequence_number
        .to_be_bytes()
        .into_iter()
        .chain(rtp_timestamp.to_be_bytes())
    {
        if payload.len() == PACKET_PAYLOAD_LEN {
            return payload;
        }
        payload.push(byte);
    }
    while payload.len() < PACKET_PAYLOAD_LEN {
        let offset = u8::try_from(payload.len()).unwrap_or(u8::MAX);
        payload.push(PAYLOAD_SEED.wrapping_add(offset));
    }
    debug_assert_eq!(payload.len() - 1, FRAME_BODY_LEN);
    payload
}

#[cfg(test)]
mod tests {
    use super::{AudioSource, FRAME_INTERVAL, PACKET_PAYLOAD_LEN, TIMESTAMP_STEP};

    #[test]
    fn audio_source_has_deterministic_rtp_timing() {
        let mut source = AudioSource::default();
        let first = source.next_packet();
        let second = source.next_packet();

        assert_eq!(first.payload.len(), PACKET_PAYLOAD_LEN);
        assert_eq!(first.emitted_at, FRAME_INTERVAL);
        assert_eq!(first.sequence_number, 0);
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.rtp_timestamp, TIMESTAMP_STEP);
    }
}
