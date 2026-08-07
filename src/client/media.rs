use std::time::Duration;

use anyhow::{Context, Result, ensure};
use o_sfu_rfc::rtp::{self, frame_marking, opus};
use str0m::rtp::ExtensionValues;

use crate::{
    AUDIO_PACKET_PAYLOAD_BYTES, VIDEO_HIGH_DELTA_PACKETS, VIDEO_HIGH_KEYFRAME_PACKETS,
    VIDEO_HIGH_PACKET_PAYLOAD_BYTES, VIDEO_KEYFRAME_INTERVAL, VIDEO_LOW_DELTA_PACKETS,
    VIDEO_LOW_KEYFRAME_PACKETS, VIDEO_LOW_PACKET_PAYLOAD_BYTES,
};

pub(super) const AUDIO_FRAME_INTERVAL: Duration = Duration::from_millis(20);
const AUDIO_TIMESTAMP_STEP: u32 = opus::RTP_CLOCK_RATE_HZ / 50;
pub(super) const VIDEO_FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);
const VIDEO_TIMESTAMP_STEP: u32 = 3_000;
const IDENTITY_MAGIC: [u8; 4] = *b"OSFU";
const IDENTITY_VERSION: u8 = 1;
const IDENTITY_HEADER_LEN: usize = 22;
const VP8_DESCRIPTOR_LEN: usize = 6;
const VP8_KEYFRAME_PREFIX_LEN: usize = 10;
const VP8_LOW_KEYFRAME_PREFIX: [u8; VP8_KEYFRAME_PREFIX_LEN] =
    [0x30, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xb4, 0x00];
const VP8_HIGH_KEYFRAME_PREFIX: [u8; VP8_KEYFRAME_PREFIX_LEN] =
    [0x30, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0x00, 0x05, 0xd0, 0x02];
const VP8_INTERFRAME_PREFIX: [u8; 1] = [rtp::vp8::INTERFRAME_BIT];
const MAX_IDENTITY_OFFSET: usize = VP8_DESCRIPTOR_LEN + VP8_KEYFRAME_PREFIX_LEN;
const OPUS_HYBRID_FULLBAND_20_MS_CONFIG: u8 = 15;
const ONE_FRAME_TOC: u8 =
    (OPUS_HYBRID_FULLBAND_20_MS_CONFIG << 3) | opus::frame_count_code::ONE_FRAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketPhase {
    Warmup,
    Measured,
}

impl PacketPhase {
    const fn code(self) -> u8 {
        match self {
            Self::Warmup => 0,
            Self::Measured => 1,
        }
    }
}

impl TryFrom<u8> for PacketPhase {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Warmup),
            1 => Ok(Self::Measured),
            _ => anyhow::bail!("unknown packet phase {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Audio,
    Video,
}

impl PayloadKind {
    const fn code(self) -> u8 {
        match self {
            Self::Audio => 1,
            Self::Video => 2,
        }
    }
}

impl TryFrom<u8> for PayloadKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Audio),
            2 => Ok(Self::Video),
            _ => anyhow::bail!("unknown payload kind {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoLayer {
    Low,
    High,
}

impl VideoLayer {
    #[must_use]
    pub const fn rid(self) -> &'static str {
        match self {
            Self::Low => "lo",
            Self::High => "hi",
        }
    }

    #[must_use]
    pub const fn packet_payload_len(self) -> usize {
        match self {
            Self::Low => VIDEO_LOW_PACKET_PAYLOAD_BYTES,
            Self::High => VIDEO_HIGH_PACKET_PAYLOAD_BYTES,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::High => 2,
        }
    }
}

impl TryFrom<u8> for VideoLayer {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Low),
            2 => Ok(Self::High),
            _ => anyhow::bail!("unknown video layer {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketIdentity {
    pub kind: PayloadKind,
    pub phase: PacketPhase,
    pub layer: Option<VideoLayer>,
    pub source: u16,
    pub ordinal: u32,
    pub frame: u32,
    pub fragment: u16,
    pub fragments: u16,
}

pub struct PayloadInspection {
    pub identity: PacketIdentity,
    pub payload_matches: bool,
}

pub struct AudioPacket {
    pub emitted_at: Duration,
    pub rtp_timestamp: u32,
    pub sequence_number: u16,
    pub extension_values: ExtensionValues,
    pub payload: Vec<u8>,
}

pub struct VideoPacket {
    pub emitted_at: Duration,
    pub rtp_timestamp: u32,
    pub sequence_number: u16,
    pub marker: bool,
    pub layer: VideoLayer,
    pub extension_values: ExtensionValues,
    pub payload: Vec<u8>,
}

pub struct AudioSource {
    source: u16,
    timeline: MediaTimeline,
    next_rtp_timestamp: u32,
    next_sequence_number: u16,
}

struct MediaTimeline {
    first_emitted_at: Duration,
    next_emitted_at: Duration,
    interval: Duration,
}

impl MediaTimeline {
    const fn new(interval: Duration) -> Self {
        Self {
            first_emitted_at: interval,
            next_emitted_at: interval,
            interval,
        }
    }

    fn staggered(interval: Duration, source_index: u32, source_count: u32) -> Self {
        let source_count = source_count.max(1);
        let slot = source_index.saturating_add(1).min(source_count);
        let first_emitted_at = interval
            .checked_div(source_count)
            .unwrap_or(interval)
            .saturating_mul(slot);
        Self {
            first_emitted_at,
            next_emitted_at: first_emitted_at,
            interval,
        }
    }

    const fn next_emitted_at(&self) -> Duration {
        self.next_emitted_at
    }

    fn advance(&mut self) -> Duration {
        let emitted_at = self.next_emitted_at;
        self.next_emitted_at = self.next_emitted_at.saturating_add(self.interval);
        emitted_at
    }

    fn reset(&mut self) {
        self.next_emitted_at = self.first_emitted_at;
    }
}

impl AudioSource {
    #[must_use]
    pub const fn new(source: u16) -> Self {
        Self {
            source,
            timeline: MediaTimeline::new(AUDIO_FRAME_INTERVAL),
            next_rtp_timestamp: 0,
            next_sequence_number: 0,
        }
    }

    #[must_use]
    pub fn staggered(source: u16, source_index: u32, source_count: u32) -> Self {
        Self {
            timeline: MediaTimeline::staggered(AUDIO_FRAME_INTERVAL, source_index, source_count),
            ..Self::new(source)
        }
    }

    #[must_use]
    pub const fn next_emitted_at(&self) -> Duration {
        self.timeline.next_emitted_at()
    }

    pub fn next_packet(&mut self, phase: PacketPhase, ordinal: u32) -> AudioPacket {
        let emitted_at = self.timeline.advance();
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number = self.next_sequence_number.wrapping_add(1);
        let rtp_timestamp = self.next_rtp_timestamp;
        self.next_rtp_timestamp = self.next_rtp_timestamp.wrapping_add(AUDIO_TIMESTAMP_STEP);
        let identity = PacketIdentity {
            kind: PayloadKind::Audio,
            phase,
            layer: None,
            source: self.source,
            ordinal,
            frame: ordinal,
            fragment: 0,
            fragments: 1,
        };
        AudioPacket {
            emitted_at,
            rtp_timestamp,
            sequence_number,
            extension_values: ExtensionValues {
                audio_level: Some(-32),
                voice_activity: Some(true),
                ..ExtensionValues::default()
            },
            payload: encoded_payload(identity, AUDIO_PACKET_PAYLOAD_BYTES, &[ONE_FRAME_TOC]),
        }
    }

    pub fn reset_timeline(&mut self) {
        self.timeline.reset();
    }
}

pub struct VideoSource {
    source: u16,
    timeline: MediaTimeline,
    next_rtp_timestamp: u32,
    low: VideoLayerState,
    high: VideoLayerState,
    measured_low_ordinal: u32,
    measured_high_ordinal: u32,
    warmup_low_ordinal: u32,
    warmup_high_ordinal: u32,
}

struct VideoLayerState {
    sequence_number: u16,
    picture_id: u16,
    tl0_pic_idx: u8,
}

impl VideoLayerState {
    const fn new(sequence_offset: u16) -> Self {
        Self {
            sequence_number: sequence_offset,
            picture_id: 1,
            tl0_pic_idx: 1,
        }
    }
}

impl VideoSource {
    #[must_use]
    pub const fn new(source: u16) -> Self {
        Self {
            source,
            timeline: MediaTimeline::new(VIDEO_FRAME_INTERVAL),
            next_rtp_timestamp: 0,
            low: VideoLayerState::new(0),
            high: VideoLayerState::new(30_000),
            measured_low_ordinal: 0,
            measured_high_ordinal: 0,
            warmup_low_ordinal: 0,
            warmup_high_ordinal: 0,
        }
    }

    #[must_use]
    pub fn staggered(source: u16, source_index: u32, source_count: u32) -> Self {
        Self {
            timeline: MediaTimeline::staggered(VIDEO_FRAME_INTERVAL, source_index, source_count),
            ..Self::new(source)
        }
    }

    #[must_use]
    pub const fn next_emitted_at(&self) -> Duration {
        self.timeline.next_emitted_at()
    }

    pub fn next_frame(&mut self, phase: PacketPhase, frame: u32) -> Vec<VideoPacket> {
        let emitted_at = self.timeline.advance();
        let timestamp = self.next_rtp_timestamp;
        self.next_rtp_timestamp = self.next_rtp_timestamp.wrapping_add(VIDEO_TIMESTAMP_STEP);
        let keyframe = u64::from(frame).is_multiple_of(VIDEO_KEYFRAME_INTERVAL);
        let low_fragments = if keyframe {
            VIDEO_LOW_KEYFRAME_PACKETS
        } else {
            VIDEO_LOW_DELTA_PACKETS
        };
        let high_fragments = if keyframe {
            VIDEO_HIGH_KEYFRAME_PACKETS
        } else {
            VIDEO_HIGH_DELTA_PACKETS
        };
        let capacity =
            usize::try_from(low_fragments.saturating_add(high_fragments)).unwrap_or(usize::MAX);
        let mut packets = Vec::with_capacity(capacity);
        append_video_layer(
            &mut packets,
            self.source,
            emitted_at,
            timestamp,
            phase,
            frame,
            keyframe,
            VideoLayer::Low,
            low_fragments,
            &mut self.low,
            match phase {
                PacketPhase::Warmup => &mut self.warmup_low_ordinal,
                PacketPhase::Measured => &mut self.measured_low_ordinal,
            },
        );
        append_video_layer(
            &mut packets,
            self.source,
            emitted_at,
            timestamp,
            phase,
            frame,
            keyframe,
            VideoLayer::High,
            high_fragments,
            &mut self.high,
            match phase {
                PacketPhase::Warmup => &mut self.warmup_high_ordinal,
                PacketPhase::Measured => &mut self.measured_high_ordinal,
            },
        );
        packets
    }

    pub fn reset_timeline(&mut self) {
        self.timeline.reset();
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "all RTP frame and layer coordinates are required to build one deterministic burst"
)]
fn append_video_layer(
    packets: &mut Vec<VideoPacket>,
    source: u16,
    emitted_at: Duration,
    rtp_timestamp: u32,
    phase: PacketPhase,
    frame: u32,
    keyframe: bool,
    layer: VideoLayer,
    fragment_count: u64,
    state: &mut VideoLayerState,
    next_ordinal: &mut u32,
) {
    let picture_id = state.picture_id & rtp::vp8::LONG_PICTURE_ID_MASK;
    let tl0_pic_idx = state.tl0_pic_idx;
    state.picture_id = state.picture_id.wrapping_add(1);
    state.tl0_pic_idx = state.tl0_pic_idx.wrapping_add(1);
    let fragments = u16::try_from(fragment_count).unwrap_or(u16::MAX);
    for fragment in 0..fragments {
        let marker = fragment + 1 == fragments;
        let identity = PacketIdentity {
            kind: PayloadKind::Video,
            phase,
            layer: Some(layer),
            source,
            ordinal: *next_ordinal,
            frame,
            fragment,
            fragments,
        };
        *next_ordinal = next_ordinal.wrapping_add(1);
        let descriptor = vp8_descriptor(picture_id, tl0_pic_idx, fragment == 0);
        let prefix = if fragment == 0 {
            let frame_prefix: &[u8] = if keyframe {
                match layer {
                    VideoLayer::Low => &VP8_LOW_KEYFRAME_PREFIX,
                    VideoLayer::High => &VP8_HIGH_KEYFRAME_PREFIX,
                }
            } else {
                &VP8_INTERFRAME_PREFIX
            };
            descriptor
                .into_iter()
                .chain(frame_prefix.iter().copied())
                .collect()
        } else {
            descriptor.to_vec()
        };
        let independent = keyframe && fragment == 0;
        let mut frame_mark = u32::from(frame_marking::BASE_LAYER_ID) << 24;
        if fragment == 0 {
            frame_mark |= u32::from(frame_marking::START_OF_FRAME_MASK) << 24;
        }
        if marker {
            frame_mark |= u32::from(frame_marking::END_OF_FRAME_MASK) << 24;
        }
        if independent {
            frame_mark |= u32::from(frame_marking::INDEPENDENT_FRAME_MASK) << 24;
        }
        packets.push(VideoPacket {
            emitted_at,
            rtp_timestamp,
            sequence_number: state.sequence_number,
            marker,
            layer,
            extension_values: ExtensionValues {
                frame_mark: Some(frame_mark),
                ..ExtensionValues::default()
            },
            payload: encoded_payload(identity, layer.packet_payload_len(), &prefix),
        });
        state.sequence_number = state.sequence_number.wrapping_add(1);
    }
}

fn vp8_descriptor(picture_id: u16, tl0_pic_idx: u8, start: bool) -> [u8; VP8_DESCRIPTOR_LEN] {
    let start_bit = if start { rtp::vp8::S_BIT } else { 0 };
    [
        rtp::vp8::X_BIT | start_bit,
        rtp::vp8::I_BIT | rtp::vp8::L_BIT | rtp::vp8::T_BIT,
        rtp::vp8::LONG_PICTURE_ID_BIT | u8::try_from(picture_id >> 8).unwrap_or_default(),
        u8::try_from(picture_id & 0xff).unwrap_or_default(),
        tl0_pic_idx,
        0,
    ]
}

fn encoded_payload(identity: PacketIdentity, payload_len: usize, prefix: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(payload_len);
    payload.extend_from_slice(prefix);
    payload.extend_from_slice(&IDENTITY_MAGIC);
    payload.push(IDENTITY_VERSION);
    payload.push(identity.kind.code());
    payload.push(identity.phase.code());
    payload.push(identity.layer.map_or(0, VideoLayer::code));
    payload.extend_from_slice(&identity.source.to_be_bytes());
    payload.extend_from_slice(&identity.ordinal.to_be_bytes());
    payload.extend_from_slice(&identity.frame.to_be_bytes());
    payload.extend_from_slice(&identity.fragment.to_be_bytes());
    payload.extend_from_slice(&identity.fragments.to_be_bytes());
    while payload.len() < payload_len {
        payload.push(payload_byte(identity, payload.len()));
    }
    payload.truncate(payload_len);
    payload
}

/// Extracts the immutable load identity and validates its deterministic body.
///
/// # Errors
///
/// Returns an error when the payload has no complete supported identity.
pub fn inspect_payload(payload: &[u8]) -> Result<PayloadInspection> {
    let identity_offset = payload
        .windows(IDENTITY_MAGIC.len())
        .take(MAX_IDENTITY_OFFSET + 1)
        .position(|window| window == IDENTITY_MAGIC)
        .context("RTP payload has no load identity")?;
    let header = payload
        .get(identity_offset..identity_offset.saturating_add(IDENTITY_HEADER_LEN))
        .context("RTP payload has a truncated load identity")?;
    ensure!(
        header.get(4) == Some(&IDENTITY_VERSION),
        "unsupported load identity version"
    );
    let identity = PacketIdentity {
        kind: PayloadKind::try_from(*header.get(5).context("payload kind is missing")?)?,
        phase: PacketPhase::try_from(*header.get(6).context("packet phase is missing")?)?,
        layer: match *header.get(7).context("video layer is missing")? {
            0 => None,
            layer => Some(VideoLayer::try_from(layer)?),
        },
        source: u16::from_be_bytes(read_array(header, 8)?),
        ordinal: u32::from_be_bytes(read_array(header, 10)?),
        frame: u32::from_be_bytes(read_array(header, 14)?),
        fragment: u16::from_be_bytes(read_array(header, 18)?),
        fragments: u16::from_be_bytes(read_array(header, 20)?),
    };
    let expected_len = match (identity.kind, identity.layer) {
        (PayloadKind::Audio, None) => AUDIO_PACKET_PAYLOAD_BYTES,
        (PayloadKind::Video, Some(layer)) => layer.packet_payload_len(),
        _ => anyhow::bail!("payload kind and layer disagree"),
    };
    let body_matches = payload
        .get(identity_offset.saturating_add(IDENTITY_HEADER_LEN)..)
        .is_some_and(|body| {
            body.iter().enumerate().all(|(offset, byte)| {
                *byte
                    == payload_byte(
                        identity,
                        identity_offset
                            .saturating_add(IDENTITY_HEADER_LEN)
                            .saturating_add(offset),
                    )
            })
        });
    Ok(PayloadInspection {
        identity,
        payload_matches: payload.len() == expected_len && body_matches,
    })
}

fn read_array<const N: usize>(payload: &[u8], offset: usize) -> Result<[u8; N]> {
    payload
        .get(offset..offset.saturating_add(N))
        .context("load identity field is truncated")?
        .try_into()
        .context("load identity field has an invalid length")
}

fn payload_byte(identity: PacketIdentity, offset: usize) -> u8 {
    let offset = u8::try_from(offset).unwrap_or(u8::MAX);
    let ordinal = identity.ordinal.to_le_bytes();
    let frame = identity.frame.to_le_bytes();
    0x41_u8
        .wrapping_add(offset)
        .wrapping_add(identity.source.to_le_bytes()[0])
        .wrapping_add(ordinal[0])
        .wrapping_add(ordinal[2])
        .wrapping_add(frame[1])
        .wrapping_add(identity.fragment.to_le_bytes()[0])
}

#[cfg(test)]
#[path = "../TESTS/media_tests.rs"]
mod tests;
