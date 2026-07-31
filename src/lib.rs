pub mod client;
pub mod controller;
pub mod report;
pub mod telemetry;

mod model;

pub use model::{
    AUDIO_PACKET_PAYLOAD_BYTES, AUDIO_PACKETS_PER_SECOND, CorrectnessSummary,
    RESULT_SCHEMA_VERSION, RunObservation, ScenarioResult, ScenarioSpec, ServerPolicy,
    VIDEO_FRAMES_PER_SECOND, VIDEO_HIGH_DELTA_PACKETS, VIDEO_HIGH_KEYFRAME_PACKETS,
    VIDEO_HIGH_PACKET_PAYLOAD_BYTES, VIDEO_KEYFRAME_INTERVAL, VIDEO_LOW_DELTA_PACKETS,
    VIDEO_LOW_KEYFRAME_PACKETS, VIDEO_LOW_PACKET_PAYLOAD_BYTES, WorkloadPlan,
    video_packets_per_layer,
};

pub const AUTH_KEY: &str = "u6bsUQEWrHdKIuYplirRnbBmLbrKV5PxKG7DtA71mng=";
pub const O_SFU_REVISION: &str = env!("O_SFU_LOCKED_REVISION");
pub const ROOM_ISSUER: &str = "o-sfu-load-testing";
pub const ROOM_KEY: &str = "Y2hhbm5lbC1rZXk=";
