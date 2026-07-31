use std::time::Duration;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::O_SFU_REVISION;

pub const RESULT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioSpec {
    receivers: u32,
    packets: u32,
}

impl ScenarioSpec {
    /// Builds a bounded one-room audio fanout scenario.
    ///
    /// # Errors
    ///
    /// Returns an error when the scenario has no work or exceeds o-sfu's
    /// current room-size contract.
    pub fn new(receivers: u32, packets: u32) -> Result<Self> {
        ensure!(
            (1..=99).contains(&receivers),
            "receivers must be between 1 and 99"
        );
        ensure!(packets > 0, "packets must be greater than zero");
        Ok(Self { receivers, packets })
    }

    #[must_use]
    pub const fn receivers(self) -> u32 {
        self.receivers
    }

    #[must_use]
    pub const fn packets(self) -> u32 {
        self.packets
    }

    #[must_use]
    pub fn expected_deliveries(self) -> u64 {
        u64::from(self.receivers) * u64::from(self.packets)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioResult {
    pub schema_version: u32,
    pub scenario: String,
    pub o_sfu_revision: Option<String>,
    pub receivers: u32,
    pub offered_packets: u32,
    pub expected_deliveries: u64,
    pub delivered_packets: u64,
    pub delivery_parts_per_million: u32,
    pub elapsed_ms: u64,
    pub achieved_deliveries_per_second: u64,
    pub sender_payload_sha256: String,
    pub receiver_payload_sha256: Vec<String>,
}

impl ScenarioResult {
    #[must_use]
    pub fn fixed_audio_fanout(
        spec: ScenarioSpec,
        elapsed: Duration,
        delivered_packets: u64,
        sender_payload_sha256: String,
        receiver_payload_sha256: Vec<String>,
    ) -> Self {
        let expected_deliveries = spec.expected_deliveries();
        let elapsed_ms = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let delivery_parts_per_million = delivered_packets
            .saturating_mul(1_000_000)
            .checked_div(expected_deliveries)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(u32::MAX);
        let achieved_deliveries_per_second = delivered_packets
            .saturating_mul(1_000)
            .checked_div(elapsed_ms)
            .unwrap_or_default();
        Self {
            schema_version: RESULT_SCHEMA_VERSION,
            scenario: "audio-fanout-foundation-v1".to_owned(),
            o_sfu_revision: Some(O_SFU_REVISION.to_owned()),
            receivers: spec.receivers(),
            offered_packets: spec.packets(),
            expected_deliveries,
            delivered_packets,
            delivery_parts_per_million,
            elapsed_ms,
            achieved_deliveries_per_second,
            sender_payload_sha256,
            receiver_payload_sha256,
        }
    }

    /// Validates the fixed-work result and every receiver payload digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the result schema or any correctness invariant
    /// differs from the requested scenario.
    pub fn validate(&self, spec: ScenarioSpec) -> Result<()> {
        ensure!(
            self.schema_version == RESULT_SCHEMA_VERSION,
            "unsupported result schema {}",
            self.schema_version
        );
        ensure!(self.receivers == spec.receivers(), "receiver count changed");
        ensure!(
            self.offered_packets == spec.packets(),
            "offered packet count changed"
        );
        ensure!(
            self.expected_deliveries == spec.expected_deliveries(),
            "expected delivery count changed"
        );
        ensure!(
            self.delivered_packets == self.expected_deliveries,
            "delivered {} of {} packets",
            self.delivered_packets,
            self.expected_deliveries
        );
        ensure!(
            self.receiver_payload_sha256.len()
                == usize::try_from(self.receivers).unwrap_or(usize::MAX),
            "receiver digest count changed"
        );
        ensure!(
            self.receiver_payload_sha256
                .iter()
                .all(|digest| digest == &self.sender_payload_sha256),
            "receiver payload digest differs from the publisher"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ScenarioResult, ScenarioSpec};

    #[test]
    fn result_validates_exact_fixed_work() -> anyhow::Result<()> {
        let spec = ScenarioSpec::new(2, 50)?;
        let digest = "abc".to_owned();
        let result = ScenarioResult::fixed_audio_fanout(
            spec,
            Duration::from_secs(1),
            100,
            digest.clone(),
            vec![digest.clone(), digest],
        );

        result.validate(spec)
    }

    #[test]
    fn scenario_rejects_empty_work() {
        assert!(ScenarioSpec::new(1, 0).is_err());
        assert!(ScenarioSpec::new(0, 1).is_err());
    }
}
