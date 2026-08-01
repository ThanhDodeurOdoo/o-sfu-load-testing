#[cfg(not(target_os = "linux"))]
use std::future::{self, Ready};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use std::{fs::File, io::ErrorKind, process::Stdio};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use tokio::process::Command;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    time::timeout,
};

use super::{CALL_GRAPH, EVENT, FREQUENCY_HZ};
#[cfg(target_os = "linux")]
use super::{CAPTURE_FILE, PERF_DATA_FILE, PROFILE_READY_FILE};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CaptureMetadata {
    pub(super) schema_version: u32,
    pub(super) event: String,
    pub(super) frequency_hz: u32,
    pub(super) call_graph: String,
    pub(super) duration_ms: u64,
}

pub(crate) struct ServerProfiler {
    child: Child,
    control: Option<ChildStdin>,
    acknowledgements: BufReader<ChildStdout>,
    data_path: PathBuf,
    metadata_path: PathBuf,
    started: Instant,
}

impl ServerProfiler {
    #[cfg(target_os = "linux")]
    pub(crate) async fn start(server_pid: u32, output: &Path) -> Result<Self> {
        ensure!(server_pid > 0, "o-sfu process has an invalid process id");
        let data_path = output.join(PERF_DATA_FILE);
        let metadata_path = output.join(CAPTURE_FILE);
        remove_stale(&output.join(PROFILE_READY_FILE))?;
        remove_stale(&data_path)?;
        remove_stale(&metadata_path)?;
        let stderr = File::create(output.join("perf.stderr.log"))
            .context("failed to create the perf stderr log")?;
        let mut child = Command::new("perf")
            .arg("record")
            .arg("--delay=-1")
            .arg("--control=fd:0,1")
            .arg("--event")
            .arg(EVENT)
            .arg("--freq")
            .arg(FREQUENCY_HZ.to_string())
            .arg("--call-graph")
            .arg(CALL_GRAPH)
            .arg("--strict-freq")
            .arg("--pid")
            .arg(server_pid.to_string())
            .arg("--inherit")
            .arg("--output")
            .arg(&data_path)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("failed to start perf record")?;
        let control = child.stdin.take().context("perf has no control input")?;
        let acknowledgements = child.stdout.take().context("perf has no ACK output")?;
        let mut profiler = Self {
            child,
            control: Some(control),
            acknowledgements: BufReader::new(acknowledgements),
            data_path,
            metadata_path,
            started: Instant::now(),
        };
        profiler.send_control("enable").await?;
        profiler.started = Instant::now();
        Ok(profiler)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn start(_server_pid: u32, _output: &Path) -> Ready<Result<Self>> {
        future::ready(Err(anyhow!("server profiling requires Linux perf")))
    }

    pub(crate) async fn finish(mut self) -> Result<()> {
        self.send_control("disable").await?;
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.send_control("stop").await?;
        drop(self.control.take());
        let status = match timeout(STOP_TIMEOUT, self.child.wait()).await {
            Ok(status) => status.context("failed to wait for perf record")?,
            Err(_elapsed) => {
                self.child
                    .kill()
                    .await
                    .context("failed to force-stop perf record")?;
                return Err(anyhow!("perf record exceeded its shutdown deadline"));
            }
        };
        ensure!(status.success(), "perf record exited with {status}");
        let data_size = fs::metadata(&self.data_path)
            .with_context(|| format!("failed to inspect {}", self.data_path.display()))?
            .len();
        ensure!(data_size > 0, "perf record produced an empty perf.data");
        let metadata = CaptureMetadata {
            schema_version: 1,
            event: EVENT.to_owned(),
            frequency_hz: FREQUENCY_HZ,
            call_graph: CALL_GRAPH.to_owned(),
            duration_ms,
        };
        let payload =
            serde_json::to_vec_pretty(&metadata).context("failed to encode profile metadata")?;
        fs::write(&self.metadata_path, payload)
            .with_context(|| format!("failed to write {}", self.metadata_path.display()))
    }

    async fn send_control(&mut self, command: &str) -> Result<()> {
        let control = self
            .control
            .as_mut()
            .context("perf control input is closed")?;
        control
            .write_all(format!("{command}\n").as_bytes())
            .await
            .with_context(|| format!("failed to send perf {command}"))?;
        control
            .flush()
            .await
            .with_context(|| format!("failed to flush perf {command}"))?;
        timeout(CONTROL_TIMEOUT, read_ack(&mut self.acknowledgements))
            .await
            .with_context(|| format!("perf did not acknowledge {command} before the deadline"))?
    }
}

#[cfg(target_os = "linux")]
fn remove_stale(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

async fn read_ack(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<()> {
    let mut response = Vec::new();
    let read = reader
        .read_until(b'\n', &mut response)
        .await
        .context("failed to read a perf acknowledgement")?;
    ensure!(read > 0, "perf closed its acknowledgement channel");
    let response = response.strip_suffix(b"\n").unwrap_or(&response);
    let response = response.strip_suffix(b"\0").unwrap_or(response);
    let response = response.strip_prefix(b"\0").unwrap_or(response);
    ensure!(
        response == b"ack",
        "perf returned an invalid acknowledgement"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    use super::read_ack;

    #[tokio::test]
    async fn acknowledgements_accept_documented_and_nul_terminated_frames() -> anyhow::Result<()> {
        let (mut writer, reader) = duplex(32);
        writer.write_all(b"ack\n\0ack\n").await?;
        let mut reader = BufReader::new(reader);

        read_ack(&mut reader).await?;
        read_ack(&mut reader).await?;
        Ok(())
    }

    #[tokio::test]
    async fn acknowledgement_rejects_eof_and_other_responses() -> anyhow::Result<()> {
        let (mut writer, reader) = duplex(32);
        writer.write_all(b"failed\n").await?;
        drop(writer);
        let mut reader = BufReader::new(reader);

        assert!(read_ack(&mut reader).await.is_err());
        assert!(read_ack(&mut reader).await.is_err());
        Ok(())
    }
}
