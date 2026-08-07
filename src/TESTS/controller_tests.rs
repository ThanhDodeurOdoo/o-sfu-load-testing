use std::time::Duration;

use tokio::io::BufReader;

use super::{MAX_PHASE_FRAME_BYTES, read_phase_frame, rtc_worker_deadline};
use crate::ScenarioSpec;

#[test]
fn mixed_deadline_tracks_sequential_publication_rounds() -> anyhow::Result<()> {
    let spec = ScenarioSpec::mixed_conference(1, 20, 5, 4, 10)?;

    assert_eq!(rtc_worker_deadline(spec), Duration::from_secs(250));
    Ok(())
}

#[tokio::test]
async fn phase_frames_are_line_delimited_and_bounded() -> anyhow::Result<()> {
    let payload = b"{\"phase\":\"setup\"}\n{\"phase\":\"warmup\"}\n";
    let mut input = BufReader::new(payload.as_slice());

    assert_eq!(
        read_phase_frame(&mut input).await?,
        Some(b"{\"phase\":\"setup\"}\n".to_vec())
    );
    assert_eq!(
        read_phase_frame(&mut input).await?,
        Some(b"{\"phase\":\"warmup\"}\n".to_vec())
    );
    assert_eq!(read_phase_frame(&mut input).await?, None);

    let oversized = vec![b'x'; MAX_PHASE_FRAME_BYTES + 1];
    assert!(
        read_phase_frame(&mut BufReader::new(oversized.as_slice()))
            .await
            .is_err()
    );
    assert!(
        read_phase_frame(&mut BufReader::new(b"unterminated".as_slice()))
            .await
            .is_err()
    );
    Ok(())
}
