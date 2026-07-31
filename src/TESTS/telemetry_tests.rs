use std::{env::temp_dir, net::Ipv4Addr, process};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    time::timeout,
};

use super::*;

const METRICS: &str = r#"
# HELP osfu_rtp_packets_total Total RTP packets processed by flow direction.
osfu_rtp_payload_bytes_total{direction="egress"} 2200
osfu_rtp_packets_total{direction="ingress"} 11
osfu_rtp_forwarded_packets_total{destination="recording"} 0
osfu_rtp_forwarded_payload_bytes_total{destination="local_rtc"} 2200
osfu_rtp_payload_bytes_total{direction="ingress"} 1100
osfu_rtp_forwarded_packets_total{destination="local_rtc"} 22
osfu_rtp_packets_total{direction="egress"} 22
"#;

#[test]
fn process_stat_extracts_cpu_identity_and_rss() -> Result<()> {
    let sample = parse_process_stat(
        "42 (rtc worker (load)) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21",
        4096,
    )?;

    assert_eq!(
        sample,
        ProcessSample {
            cpu_ticks: 23,
            start_time_ticks: 19,
            rss_bytes: 21 * 4096,
        }
    );
    Ok(())
}

#[test]
fn process_stat_rejects_negative_rss() {
    let result = parse_process_stat(
        "42 (worker) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 -1",
        4096,
    );

    assert!(result.is_err());
}

#[test]
fn system_parameters_parse_one_getconf_snapshot() -> Result<()> {
    assert_eq!(
        parse_system_parameters("CLK_TCK: 100\nPAGESIZE: 4096\nPAGE_SIZE: 4096\n")?
            .clock_ticks_per_second,
        100
    );
    assert_eq!(
        parse_system_parameters("CLK_TCK: 100\nPAGESIZE: 4096\n")?.page_size,
        4096
    );
    Ok(())
}

#[test]
fn traffic_parser_selects_required_counters() -> Result<()> {
    assert_eq!(
        parse_traffic(METRICS)?,
        TrafficSample {
            ingress: RtpCounters {
                packets: 11,
                payload_bytes: 1100,
            },
            egress: RtpCounters {
                packets: 22,
                payload_bytes: 2200,
            },
            forwarded_local_rtc: RtpCounters {
                packets: 22,
                payload_bytes: 2200,
            },
        }
    );
    Ok(())
}

#[test]
fn traffic_parser_rejects_missing_and_duplicate_series() {
    assert!(parse_traffic("osfu_rtp_packets_total{direction=\"ingress\"} 1\n").is_err());
    assert!(parse_traffic(&format!("{METRICS}{RTP_PACKETS_INGRESS} 12\n")).is_err());
}

#[test]
fn cpu_rate_reports_milli_percent_across_logical_cores() -> Result<()> {
    let previous = TimedProcessSample {
        elapsed_ms: 1000,
        process: ProcessSample {
            cpu_ticks: 20,
            rss_bytes: 4096,
            start_time_ticks: 5,
        },
    };
    let current = ProcessSample {
        cpu_ticks: 320,
        rss_bytes: 8192,
        start_time_ticks: 5,
    };

    assert_eq!(
        cpu_percent_milli(Some(previous), 2000, current, 100)?,
        Some(300_000)
    );
    assert_eq!(cpu_percent_milli(None, 2000, current, 100)?, None);
    Ok(())
}

#[test]
fn cpu_rate_rejects_counter_reset_and_pid_reuse() {
    let previous = TimedProcessSample {
        elapsed_ms: 1000,
        process: ProcessSample {
            cpu_ticks: 20,
            rss_bytes: 4096,
            start_time_ticks: 5,
        },
    };

    assert!(
        cpu_percent_milli(
            Some(previous),
            2000,
            ProcessSample {
                cpu_ticks: 19,
                rss_bytes: 4096,
                start_time_ticks: 5,
            },
            100,
        )
        .is_err()
    );
    assert!(
        cpu_percent_milli(
            Some(previous),
            2000,
            ProcessSample {
                cpu_ticks: 21,
                rss_bytes: 4096,
                start_time_ticks: 6,
            },
            100,
        )
        .is_err()
    );
}

#[test]
fn process_identity_rejects_pid_reuse() -> Result<()> {
    let process = ProcessSample {
        cpu_ticks: 1,
        rss_bytes: 4096,
        start_time_ticks: 5,
    };
    let mut expected = None;
    verify_start_time(&mut expected, process, "server")?;

    assert!(
        verify_start_time(
            &mut expected,
            ProcessSample {
                start_time_ticks: 6,
                ..process
            },
            "server",
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn telemetry_record_preserves_unavailable_loop_delay() -> Result<()> {
    let record = TelemetryRecord {
        elapsed_ms: 1000,
        scrape_duration_ms: 2,
        final_sample: false,
        outcome: TelemetryOutcome::Sample {
            clock_ticks_per_second: 100,
            server_cpu_percent_milli: Some(100_000),
            rtc_cpu_percent_milli: None,
            server_rss_bytes: 4096,
            rtc_rss_bytes: None,
            server: ProcessSample {
                cpu_ticks: 10,
                rss_bytes: 4096,
                start_time_ticks: 5,
            },
            rtc: None,
            traffic: TrafficSample::default(),
            workers: vec![WorkerPressureSample {
                media_worker_id: 0,
                egress_bitrate_bps: 0,
                packet_loop_delay_ms: None,
                command_backlog_depth: 0,
                relay_mailbox_depth: 0,
                worker_pressure_score: 0,
            }],
        },
    };
    let value = serde_json::to_value(record)?;
    let workers = value
        .get("workers")
        .and_then(serde_json::Value::as_array)
        .context("workers should be serialized")?;
    let worker = workers.first().context("one worker should be serialized")?;

    assert_eq!(
        value
            .get("serverCpuPercentMilli")
            .and_then(serde_json::Value::as_u64),
        Some(100_000)
    );
    assert_eq!(
        value
            .get("serverRssBytes")
            .and_then(serde_json::Value::as_u64),
        Some(4096)
    );
    assert!(
        worker
            .get("packetLoopDelayMs")
            .is_some_and(serde_json::Value::is_null)
    );
    Ok(())
}

#[tokio::test]
async fn sampler_records_immediate_and_final_samples() -> Result<()> {
    if cfg!(not(target_os = "linux")) {
        return Ok(());
    }
    let (summary, records) = run_sampler_with_mock("success", false).await?;
    let mut records = records.into_iter();
    let first = records.next().context("immediate sample is missing")?;
    let final_sample = records.next().context("final sample is missing")?;

    assert_eq!(summary.sample_count, 2);
    assert_eq!(summary.error_count, 0);
    assert!(summary.errors.is_empty());
    assert!(!first.final_sample);
    assert!(final_sample.final_sample);
    assert!(records.next().is_none());
    Ok(())
}

#[tokio::test]
async fn sampler_retains_failure_and_reaches_final_sample() -> Result<()> {
    if cfg!(not(target_os = "linux")) {
        return Ok(());
    }
    let (summary, records) = run_sampler_with_mock("recovery", true).await?;
    let mut records = records.into_iter();
    let first = records.next().context("immediate sample is missing")?;
    let final_sample = records.next().context("final sample is missing")?;

    assert_eq!(summary.sample_count, 1);
    assert_eq!(summary.error_count, 1);
    assert_eq!(summary.errors.len(), 1);
    assert!(matches!(first.outcome, TelemetryOutcome::Error { .. }));
    assert!(!first.final_sample);
    assert!(matches!(
        final_sample.outcome,
        TelemetryOutcome::Sample { .. }
    ));
    assert!(final_sample.final_sample);
    assert!(records.next().is_none());
    Ok(())
}

async fn run_sampler_with_mock(
    name: &str,
    fail_first_metrics: bool,
) -> Result<(TelemetrySummary, Vec<TelemetryRecord>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(serve_telemetry(listener, fail_first_metrics));
    let output_path = temp_dir().join(format!(
        "o-sfu-load-telemetry-{}-{name}.jsonl",
        process::id()
    ));
    let pid = process::id();

    let sampler = TelemetrySampler::start(TelemetryConfig::new(
        format!("http://{address}"),
        pid,
        pid,
        &output_path,
    ))
    .await?;
    let summary = sampler.finish().await?;
    let server_result = timeout(Duration::from_secs(5), server)
        .await
        .context("mock telemetry server timed out")?;
    server_result.context("mock telemetry server task failed")??;
    let payload = fs::read_to_string(&output_path).await?;
    fs::remove_file(output_path).await?;
    let records = payload
        .lines()
        .map(serde_json::from_str::<TelemetryRecord>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    Ok((summary, records))
}

async fn serve_telemetry(listener: TcpListener, fail_first_metrics: bool) -> Result<()> {
    let mut metrics_responses = 0_u8;
    for _ in 0..4 {
        let (mut stream, _peer) = listener.accept().await?;
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let count = stream.read(&mut buffer).await?;
            ensure!(count > 0, "telemetry request ended before its headers");
            request.extend_from_slice(
                buffer
                    .get(..count)
                    .context("telemetry request read exceeded its buffer")?,
            );
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            ensure!(request.len() <= 4096, "telemetry request is too large");
        }
        let request = String::from_utf8(request)?;
        let (status, content_type, body) = if request.contains(&format!("GET {} ", metrics::PATH)) {
            let status = if fail_first_metrics && metrics_responses == 0 {
                "500 Internal Server Error"
            } else {
                "200 OK"
            };
            metrics_responses = metrics_responses.saturating_add(1);
            (status, "text/plain", METRICS)
        } else if request.contains(&format!("GET {} ", diagnostics_route::WORKERS)) {
            ("200 OK", "application/json", "[]")
        } else {
            anyhow::bail!("unexpected telemetry request")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await?;
    }
    Ok(())
}
