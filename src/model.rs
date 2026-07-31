use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::O_SFU_REVISION;

pub const RESULT_SCHEMA_VERSION: u32 = 2;
pub const AUDIO_PACKETS_PER_SECOND: u32 = 50;
pub const AUDIO_PACKET_PAYLOAD_BYTES: usize = 160;
pub const VIDEO_FRAMES_PER_SECOND: u32 = 30;
pub const VIDEO_KEYFRAME_INTERVAL: u64 = 60;
pub const VIDEO_LOW_DELTA_PACKETS: u64 = 1;
pub const VIDEO_LOW_KEYFRAME_PACKETS: u64 = 2;
pub const VIDEO_LOW_PACKET_PAYLOAD_BYTES: usize = 600;
pub const VIDEO_HIGH_DELTA_PACKETS: u64 = 14;
pub const VIDEO_HIGH_KEYFRAME_PACKETS: u64 = 20;
pub const VIDEO_HIGH_PACKET_PAYLOAD_BYTES: usize = 1_100;
const MAX_OFFERED_PACKETS: u64 = 100_000_000;
const MAX_EXPECTED_DELIVERIES: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ScenarioSpec {
    Smoke {
        receivers: u32,
        packets: u32,
    },
    AudioMesh {
        rooms: u32,
        peers: u32,
        seconds: u32,
    },
    VideoGallery {
        rooms: u32,
        peers: u32,
        publishers: u32,
        seconds: u32,
    },
}

impl ScenarioSpec {
    /// Reapplies workload bounds after deserialization.
    ///
    /// # Errors
    ///
    /// Returns an error when any topology or duration is outside the public
    /// constructor contract.
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Smoke { receivers, packets } => Self::smoke(receivers, packets)?,
            Self::AudioMesh {
                rooms,
                peers,
                seconds,
            } => Self::audio_mesh(rooms, peers, seconds)?,
            Self::VideoGallery {
                rooms,
                peers,
                publishers,
                seconds,
            } => Self::video_gallery(rooms, peers, publishers, seconds)?,
        };
        Ok(())
    }

    /// Builds the bounded one-source correctness smoke.
    ///
    /// # Errors
    ///
    /// Returns an error when the scenario has no work or exceeds o-sfu's
    /// current room-size contract.
    pub fn smoke(receivers: u32, packets: u32) -> Result<Self> {
        ensure!(
            (1..=99).contains(&receivers),
            "receivers must be between 1 and 99"
        );
        ensure!(
            (1..=180_000).contains(&packets),
            "packets must be between 1 and 180000"
        );
        Ok(Self::Smoke { receivers, packets })
    }

    /// Builds an all-to-all continuous Opus workload.
    ///
    /// # Errors
    ///
    /// Returns an error when the scenario has no work or a room exceeds
    /// o-sfu's current room-size contract.
    pub fn audio_mesh(rooms: u32, peers: u32, seconds: u32) -> Result<Self> {
        ensure!((1..=64).contains(&rooms), "rooms must be between 1 and 64");
        ensure!(
            (2..=100).contains(&peers),
            "peers must be between 2 and 100"
        );
        ensure!(
            (1..=3_600).contains(&seconds),
            "seconds must be between 1 and 3600"
        );
        let spec = Self::AudioMesh {
            rooms,
            peers,
            seconds,
        };
        spec.validate_plan_size()?;
        Ok(spec)
    }

    /// Builds a VP8 simulcast gallery with one selected layer per route.
    ///
    /// # Errors
    ///
    /// Returns an error when the scenario has no work or exceeds o-sfu's
    /// room-size and active-video-download contracts.
    pub fn video_gallery(rooms: u32, peers: u32, publishers: u32, seconds: u32) -> Result<Self> {
        ensure!((1..=64).contains(&rooms), "rooms must be between 1 and 64");
        ensure!(
            (3..=100).contains(&peers),
            "peers must be between 3 and 100"
        );
        ensure!(publishers > 0, "publishers must be greater than zero");
        ensure!(publishers <= peers, "publishers cannot exceed peers");
        ensure!(
            publishers <= 10,
            "publishers cannot exceed ten video downloads"
        );
        ensure!(
            (1..=3_600).contains(&seconds),
            "seconds must be between 1 and 3600"
        );
        let spec = Self::VideoGallery {
            rooms,
            peers,
            publishers,
            seconds,
        };
        spec.validate_plan_size()?;
        Ok(spec)
    }

    #[must_use]
    pub const fn room_count(self) -> u32 {
        match self {
            Self::Smoke { .. } => 1,
            Self::AudioMesh { rooms, .. } | Self::VideoGallery { rooms, .. } => rooms,
        }
    }

    #[must_use]
    pub const fn peers_per_room(self) -> u32 {
        match self {
            Self::Smoke { receivers, .. } => receivers + 1,
            Self::AudioMesh { peers, .. } | Self::VideoGallery { peers, .. } => peers,
        }
    }

    #[must_use]
    pub const fn publishers_per_room(self) -> u32 {
        match self {
            Self::Smoke { .. } => 1,
            Self::AudioMesh { peers, .. } => peers,
            Self::VideoGallery { publishers, .. } => publishers,
        }
    }

    #[must_use]
    pub const fn duration_seconds(self) -> u32 {
        match self {
            Self::Smoke { packets, .. } => packets.div_ceil(AUDIO_PACKETS_PER_SECOND),
            Self::AudioMesh { seconds, .. } | Self::VideoGallery { seconds, .. } => seconds,
        }
    }

    #[must_use]
    pub const fn profile(self) -> &'static str {
        match self {
            Self::Smoke { .. } => "opus-fanout-smoke-v2",
            Self::AudioMesh { .. } => "opus-20ms-audio-mesh-v1",
            Self::VideoGallery { .. } => "vp8-simulcast-gallery-v1",
        }
    }

    #[must_use]
    pub const fn active_audio_speakers(self) -> u32 {
        match self {
            Self::AudioMesh { peers, .. } => peers,
            Self::Smoke { .. } | Self::VideoGallery { .. } => 4,
        }
    }

    /// Returns exact fixed-work packet and byte cardinalities.
    ///
    /// # Errors
    ///
    /// Returns an error when a cardinality exceeds `u64`.
    pub fn plan(self) -> Result<WorkloadPlan> {
        match self {
            Self::Smoke { receivers, packets } => {
                let offered_packets = u64::from(packets);
                let routes = u64::from(receivers);
                let payload_bytes = byte_count(AUDIO_PACKET_PAYLOAD_BYTES)?;
                WorkloadPlan::new(
                    1,
                    routes,
                    offered_packets,
                    checked_mul(offered_packets, payload_bytes)?,
                    checked_mul(routes, offered_packets)?,
                    checked_mul(checked_mul(routes, offered_packets)?, payload_bytes)?,
                )
            }
            Self::AudioMesh {
                rooms,
                peers,
                seconds,
            } => {
                let rooms = u64::from(rooms);
                let peers = u64::from(peers);
                let packets_per_publisher =
                    checked_mul(u64::from(seconds), u64::from(AUDIO_PACKETS_PER_SECOND))?;
                let streams = checked_mul(rooms, peers)?;
                let remote_peers = peers
                    .checked_sub(1)
                    .ok_or_else(|| anyhow::anyhow!("audio mesh requires at least two peers"))?;
                let routes = checked_mul(streams, remote_peers)?;
                let offered_packets = checked_mul(streams, packets_per_publisher)?;
                let expected_deliveries = checked_mul(routes, packets_per_publisher)?;
                let payload_bytes = byte_count(AUDIO_PACKET_PAYLOAD_BYTES)?;
                WorkloadPlan::new(
                    streams,
                    routes,
                    offered_packets,
                    checked_mul(offered_packets, payload_bytes)?,
                    expected_deliveries,
                    checked_mul(expected_deliveries, payload_bytes)?,
                )
            }
            Self::VideoGallery {
                rooms,
                peers,
                publishers,
                seconds,
            } => video_plan(rooms, peers, publishers, seconds),
        }
    }

    fn validate_plan_size(self) -> Result<()> {
        let plan = self.plan()?;
        ensure!(
            plan.offered_packets <= MAX_OFFERED_PACKETS,
            "workload exceeds the offered packet limit"
        );
        ensure!(
            plan.expected_deliveries <= MAX_EXPECTED_DELIVERIES,
            "workload exceeds the delivery limit"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadPlan {
    pub streams: u64,
    pub routes: u64,
    pub offered_packets: u64,
    pub offered_payload_bytes: u64,
    pub expected_deliveries: u64,
    pub expected_delivery_payload_bytes: u64,
}

impl WorkloadPlan {
    fn new(
        streams: u64,
        routes: u64,
        offered_packets: u64,
        offered_payload_bytes: u64,
        expected_deliveries: u64,
        expected_delivery_payload_bytes: u64,
    ) -> Result<Self> {
        ensure!(streams > 0, "workload must contain a stream");
        ensure!(routes > 0, "workload must contain a route");
        Ok(Self {
            streams,
            routes,
            offered_packets,
            offered_payload_bytes,
            expected_deliveries,
            expected_delivery_payload_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerPolicy {
    pub media_workers: u32,
    pub room_size: u32,
    pub max_active_audio_speakers: u32,
    pub max_video_downloads_per_receiver: u32,
    pub max_bitrate_in_bps: u64,
    pub max_bitrate_out_bps: u64,
}

impl ServerPolicy {
    #[must_use]
    pub const fn for_scenario(spec: ScenarioSpec) -> Self {
        Self {
            media_workers: 1,
            room_size: spec.peers_per_room(),
            max_active_audio_speakers: spec.active_audio_speakers(),
            max_video_downloads_per_receiver: 10,
            max_bitrate_in_bps: 8_000_000,
            max_bitrate_out_bps: 10_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectnessSummary {
    pub missing_packets: u64,
    pub duplicate_packets: u64,
    pub out_of_order_packets: u64,
    pub unexpected_packets: u64,
    pub payload_mismatches: u64,
}

impl CorrectnessSummary {
    #[must_use]
    pub const fn discrepancy_count(self) -> u64 {
        self.missing_packets
            .saturating_add(self.duplicate_packets)
            .saturating_add(self.out_of_order_packets)
            .saturating_add(self.unexpected_packets)
            .saturating_add(self.payload_mismatches)
    }

    pub fn merge(&mut self, other: Self) {
        self.missing_packets = self.missing_packets.saturating_add(other.missing_packets);
        self.duplicate_packets = self
            .duplicate_packets
            .saturating_add(other.duplicate_packets);
        self.out_of_order_packets = self
            .out_of_order_packets
            .saturating_add(other.out_of_order_packets);
        self.unexpected_packets = self
            .unexpected_packets
            .saturating_add(other.unexpected_packets);
        self.payload_mismatches = self
            .payload_mismatches
            .saturating_add(other.payload_mismatches);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunObservation {
    pub offered_packets: u64,
    pub offered_payload_bytes: u64,
    pub delivered_packets: u64,
    pub delivered_payload_bytes: u64,
    pub correctness: CorrectnessSummary,
    pub elapsed_ms: u64,
    pub max_send_lag_ms: u64,
}

impl RunObservation {
    pub fn merge(&mut self, other: Self) {
        self.offered_packets = self.offered_packets.saturating_add(other.offered_packets);
        self.offered_payload_bytes = self
            .offered_payload_bytes
            .saturating_add(other.offered_payload_bytes);
        self.delivered_packets = self
            .delivered_packets
            .saturating_add(other.delivered_packets);
        self.delivered_payload_bytes = self
            .delivered_payload_bytes
            .saturating_add(other.delivered_payload_bytes);
        self.correctness.merge(other.correctness);
        self.elapsed_ms = self.elapsed_ms.max(other.elapsed_ms);
        self.max_send_lag_ms = self.max_send_lag_ms.max(other.max_send_lag_ms);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioResult {
    pub schema_version: u32,
    pub profile: String,
    pub o_sfu_revision: Option<String>,
    pub scenario: ScenarioSpec,
    pub server_policy: ServerPolicy,
    pub plan: WorkloadPlan,
    pub offered_packets: u64,
    pub offered_payload_bytes: u64,
    pub delivered_packets: u64,
    pub delivered_payload_bytes: u64,
    pub correctness: CorrectnessSummary,
    pub elapsed_ms: u64,
    pub max_send_lag_ms: u64,
}

impl ScenarioResult {
    /// Builds a result from measured fixed work.
    ///
    /// # Errors
    ///
    /// Returns an error when the scenario cardinalities overflow.
    pub fn completed(spec: ScenarioSpec, observation: RunObservation) -> Result<Self> {
        Ok(Self {
            schema_version: RESULT_SCHEMA_VERSION,
            profile: spec.profile().to_owned(),
            o_sfu_revision: Some(O_SFU_REVISION.to_owned()),
            scenario: spec,
            server_policy: ServerPolicy::for_scenario(spec),
            plan: spec.plan()?,
            offered_packets: observation.offered_packets,
            offered_payload_bytes: observation.offered_payload_bytes,
            delivered_packets: observation.delivered_packets,
            delivered_payload_bytes: observation.delivered_payload_bytes,
            correctness: observation.correctness,
            elapsed_ms: observation.elapsed_ms.max(1),
            max_send_lag_ms: observation.max_send_lag_ms,
        })
    }

    #[must_use]
    pub fn achieved_deliveries_per_second(&self) -> u64 {
        let scaled = u128::from(self.delivered_packets).saturating_mul(1_000);
        let rate = scaled
            .checked_div(u128::from(self.elapsed_ms.max(1)))
            .unwrap_or_default();
        u64::try_from(rate).unwrap_or(u64::MAX)
    }

    /// Validates every fixed-work accounting invariant.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema, scenario, workload or exact delivery
    /// differs from the requested run.
    pub fn validate(&self, spec: ScenarioSpec) -> Result<()> {
        spec.validate()?;
        let plan = spec.plan()?;
        ensure!(
            self.schema_version == RESULT_SCHEMA_VERSION,
            "unsupported result schema {}",
            self.schema_version
        );
        ensure!(self.scenario == spec, "scenario specification changed");
        ensure!(self.profile == spec.profile(), "media profile changed");
        ensure!(
            self.server_policy == ServerPolicy::for_scenario(spec),
            "server policy changed"
        );
        ensure!(self.plan == plan, "workload plan changed");
        ensure!(
            self.offered_packets == plan.offered_packets,
            "offered packet count changed"
        );
        ensure!(
            self.offered_payload_bytes == plan.offered_payload_bytes,
            "offered payload byte count changed"
        );
        ensure!(
            self.delivered_packets == plan.expected_deliveries,
            "delivered {} of {} packets",
            self.delivered_packets,
            plan.expected_deliveries
        );
        ensure!(
            self.delivered_payload_bytes == plan.expected_delivery_payload_bytes,
            "delivered payload byte count changed"
        );
        ensure!(
            self.correctness.discrepancy_count() == 0,
            "fixed work contains {} packet discrepancies",
            self.correctness.discrepancy_count()
        );
        ensure!(
            self.elapsed_ms > 0,
            "elapsed time must be greater than zero"
        );
        Ok(())
    }
}

fn video_plan(rooms: u32, peers: u32, publishers: u32, seconds: u32) -> Result<WorkloadPlan> {
    let rooms = u64::from(rooms);
    let peers = u64::from(peers);
    let publishers = u64::from(publishers);
    let (low_packets, high_packets) = video_packets_per_layer(seconds)?;
    let sources = checked_mul(rooms, publishers)?;
    let streams = checked_mul(sources, 2)?;
    let offered_low_packets = checked_mul(sources, low_packets)?;
    let offered_high_packets = checked_mul(sources, high_packets)?;
    let offered_packets = checked_add(offered_low_packets, offered_high_packets)?;
    let low_payload_bytes = byte_count(VIDEO_LOW_PACKET_PAYLOAD_BYTES)?;
    let high_payload_bytes = byte_count(VIDEO_HIGH_PACKET_PAYLOAD_BYTES)?;
    let offered_payload_bytes = checked_add(
        checked_mul(offered_low_packets, low_payload_bytes)?,
        checked_mul(offered_high_packets, high_payload_bytes)?,
    )?;
    let remote_peers = peers
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("video gallery requires at least two peers"))?;
    let routes_per_room = checked_mul(publishers, remote_peers)?;
    let high_routes_per_room = if publishers == 1 { remote_peers } else { peers };
    let low_routes_per_room = routes_per_room
        .checked_sub(high_routes_per_room)
        .ok_or_else(|| anyhow::anyhow!("video route count underflowed"))?;
    let high_routes = checked_mul(rooms, high_routes_per_room)?;
    let low_routes = checked_mul(rooms, low_routes_per_room)?;
    let expected_high_deliveries = checked_mul(high_routes, high_packets)?;
    let expected_low_deliveries = checked_mul(low_routes, low_packets)?;
    let expected_deliveries = checked_add(expected_high_deliveries, expected_low_deliveries)?;
    let expected_delivery_payload_bytes = checked_add(
        checked_mul(expected_high_deliveries, high_payload_bytes)?,
        checked_mul(expected_low_deliveries, low_payload_bytes)?,
    )?;
    WorkloadPlan::new(
        streams,
        checked_mul(rooms, routes_per_room)?,
        offered_packets,
        offered_payload_bytes,
        expected_deliveries,
        expected_delivery_payload_bytes,
    )
}

/// Returns exact low and high VP8 packet counts for one publisher.
///
/// # Errors
///
/// Returns an error when the fixed profile cardinality exceeds `u64`.
pub fn video_packets_per_layer(seconds: u32) -> Result<(u64, u64)> {
    let frames = checked_mul(u64::from(seconds), u64::from(VIDEO_FRAMES_PER_SECOND))?;
    let keyframes = frames.div_ceil(VIDEO_KEYFRAME_INTERVAL);
    let low_packets = checked_add(
        checked_mul(frames - keyframes, VIDEO_LOW_DELTA_PACKETS)?,
        checked_mul(keyframes, VIDEO_LOW_KEYFRAME_PACKETS)?,
    )?;
    let high_packets = checked_add(
        checked_mul(frames - keyframes, VIDEO_HIGH_DELTA_PACKETS)?,
        checked_mul(keyframes, VIDEO_HIGH_KEYFRAME_PACKETS)?,
    )?;
    Ok((low_packets, high_packets))
}

fn checked_add(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| anyhow::anyhow!("workload cardinality overflowed"))
}

fn checked_mul(left: u64, right: u64) -> Result<u64> {
    left.checked_mul(right)
        .ok_or_else(|| anyhow::anyhow!("workload cardinality overflowed"))
}

fn byte_count(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_error| anyhow::anyhow!("payload byte count exceeds u64"))
}

#[cfg(test)]
#[path = "TESTS/model_tests.rs"]
mod tests;
