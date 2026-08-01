mod capture;
mod report;

pub(crate) use capture::ServerProfiler;
pub use report::{prepare, render, write};

pub(crate) const CALL_GRAPH: &str = "fp";
pub(crate) const CAPTURE_FILE: &str = "profile.json";
pub(crate) const ENVIRONMENT_FILE: &str = "environment.json";
pub(crate) const EVENT: &str = "cpu-clock";
pub(crate) const FLAMEGRAPH_FILE: &str = "flamegraph.svg";
pub(crate) const FOLDED_FILE: &str = "stacks.folded";
pub(crate) const FREQUENCY_HZ: u32 = 99;
pub(crate) const PERF_DATA_FILE: &str = "perf.data";
pub(crate) const PROFILE_READY_FILE: &str = "profile.ready";
