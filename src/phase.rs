use std::{
    fmt,
    io::{self, BufReader, BufWriter},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

/// Identifies one global scenario transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioPhase {
    Setup,
    Warmup,
    Measured,
    Drain,
}

impl ScenarioPhase {
    pub(crate) const ORDERED: [Self; 4] = [Self::Setup, Self::Warmup, Self::Measured, Self::Drain];
    pub(crate) const COUNT: usize = Self::ORDERED.len();

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::Setup => 0,
            Self::Warmup => 1,
            Self::Measured => 2,
            Self::Drain => 3,
        }
    }
}

impl fmt::Display for ScenarioPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Setup => "setup",
            Self::Warmup => "warmup",
            Self::Measured => "measured",
            Self::Drain => "drain",
        })
    }
}

/// Requests persistence of one phase before RTC work continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseEvent {
    pub phase: ScenarioPhase,
}

/// Confirms that the controller persisted the matching phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseAcknowledgement {
    pub phase: ScenarioPhase,
}

/// Exchanges scenario transitions over the RTC worker control stream.
#[derive(Clone)]
pub struct PhaseReporter {
    io: Arc<Mutex<PhaseIo>>,
}

struct PhaseIo {
    input: BufReader<io::Stdin>,
    output: BufWriter<io::Stdout>,
}

impl PhaseReporter {
    /// Connects the reporter to the RTC worker's standard streams.
    #[must_use]
    pub fn stdio() -> Self {
        Self {
            io: Arc::new(Mutex::new(PhaseIo {
                input: BufReader::new(io::stdin()),
                output: BufWriter::new(io::stdout()),
            })),
        }
    }

    /// Reports one global scenario transition.
    ///
    /// # Errors
    ///
    /// Returns an error when the phase stream fails or the controller does not
    /// acknowledge the same transition.
    pub fn report(&self, phase: ScenarioPhase) -> Result<()> {
        let mut io = self
            .io
            .lock()
            .map_err(|_error| anyhow::anyhow!("RTC phase stream lock is poisoned"))?;
        let PhaseIo { input, output } = &mut *io;
        let result = exchange_phase(input, output, phase);
        drop(io);
        result
    }
}

pub(crate) fn exchange_phase(
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
    phase: ScenarioPhase,
) -> Result<()> {
    serde_json::to_writer(&mut *output, &PhaseEvent { phase })
        .context("failed to encode an RTC phase event")?;
    output
        .write_all(b"\n")
        .context("failed to terminate an RTC phase event")?;
    output
        .flush()
        .context("failed to flush an RTC phase event")?;
    let mut acknowledgement = String::new();
    let bytes = input
        .read_line(&mut acknowledgement)
        .context("failed to read an RTC phase acknowledgement")?;
    ensure!(bytes > 0, "controller closed the RTC phase stream");
    let acknowledgement = serde_json::from_str::<PhaseAcknowledgement>(&acknowledgement)
        .context("failed to decode an RTC phase acknowledgement")?;
    ensure!(
        acknowledgement.phase == phase,
        "controller acknowledged {} during {phase}",
        acknowledgement.phase
    );
    Ok(())
}

#[derive(Default)]
pub(crate) struct PhaseSequence {
    count: u8,
}

impl PhaseSequence {
    pub(crate) fn advance(&mut self, phase: ScenarioPhase) -> Result<()> {
        let expected = ScenarioPhase::ORDERED
            .get(usize::from(self.count))
            .context("scenario phase sequence is already complete")?;
        ensure!(
            phase == *expected,
            "expected {expected} phase, received {phase}"
        );
        self.count = self.count.saturating_add(1);
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<()> {
        ensure!(
            usize::from(self.count) == ScenarioPhase::COUNT,
            "scenario phase sequence is incomplete"
        );
        Ok(())
    }
}
