use std::time::Duration;

use super::rtc_worker_deadline;
use crate::ScenarioSpec;

#[test]
fn mixed_deadline_tracks_sequential_publication_rounds() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 20, 5, 4, 10)?;

    assert_eq!(rtc_worker_deadline(spec), Duration::from_secs(250));
    Ok(())
}
