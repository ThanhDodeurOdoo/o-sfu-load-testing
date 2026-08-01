use std::time::Duration;

use super::rtc_worker_deadline;
use crate::ScenarioSpec;

#[test]
fn mixed_deadline_tracks_sequential_publication_rounds() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 100, 10, 9, 60)?;

    assert_eq!(rtc_worker_deadline(spec), Duration::from_secs(500));
    Ok(())
}
