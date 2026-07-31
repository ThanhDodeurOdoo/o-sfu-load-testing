use std::{
    fmt::Write as _,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use o_sfu::{
    auth::{HttpRoomClaims, RegisteredJwtClaims, WebSocketConnectClaims, sign},
    http::{CreateRoomQuery, RoomResponse, route},
};
use o_sfu_protocol::wire::{UserId, UserPermissions};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    sync::{Barrier, watch},
    task::JoinSet,
};

use crate::{
    AUTH_KEY, ROOM_ISSUER, ROOM_KEY, ScenarioResult, ScenarioSpec,
    client::{
        media::AudioSource,
        protocol::{LoadPeer, ProtocolPeer},
    },
};

const PACKET_TIMEOUT: Duration = Duration::from_secs(10);
const QUIESCENCE_TIMEOUT: Duration = Duration::from_millis(200);
const WARMUP_TIMEOUT: Duration = Duration::from_secs(1);
const WARMUP_ATTEMPTS: u32 = 5;

/// Runs one fixed-work audio fanout scenario through production boundaries.
///
/// # Errors
///
/// Returns an error when room setup, signaling, RTC transport, exact packet
/// delivery, payload validation or result persistence fails.
pub async fn run(
    base_url: &str,
    websocket_url: &str,
    output_path: &Path,
    spec: ScenarioSpec,
) -> Result<ScenarioResult> {
    let room_id = create_room(base_url).await?;
    let publisher_token = connect_token(&room_id, UserId::Integer(1))?;
    let mut publisher = Box::pin(ProtocolPeer::connect(
        websocket_url,
        &publisher_token,
        &room_id,
    ))
    .await?;
    let mut receivers =
        Box::pin(connect_receivers(websocket_url, &room_id, spec.receivers())).await?;

    Box::pin(publisher.publish_audio()).await?;
    join_all(
        receivers
            .iter_mut()
            .map(ProtocolPeer::accept_next_negotiation),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    let mut source = AudioSource::default();
    warm_up_route(&mut publisher, &mut receivers, &mut source).await?;
    let publisher = publisher.into_load_peer()?;
    let receivers = receivers
        .into_iter()
        .map(ProtocolPeer::into_load_peer)
        .collect::<Result<Vec<_>>>()?;
    let (elapsed, sender_digest, receiver_digests, delivered_packets) =
        Box::pin(run_fixed_work(publisher, receivers, source, spec)).await?;
    let result = ScenarioResult::fixed_audio_fanout(
        spec,
        elapsed,
        delivered_packets,
        sender_digest,
        receiver_digests,
    );
    result.validate(spec)?;
    write_result(output_path, &result).await?;
    Ok(result)
}

async fn create_room(base_url: &str) -> Result<String> {
    let token = sign(
        &HttpRoomClaims {
            registered: RegisteredJwtClaims {
                iss: Some(ROOM_ISSUER.to_owned()),
                ..RegisteredJwtClaims::default()
            },
            key: Some(ROOM_KEY.to_owned()),
        },
        AUTH_KEY,
    )
    .context("failed to sign room creation claims")?;
    let response = reqwest::Client::new()
        .get(format!("{base_url}{}", route::v1::CHANNEL))
        .bearer_auth(token)
        .header("x-forwarded-for", "127.0.0.1")
        .query(&CreateRoomQuery::default())
        .send()
        .await
        .context("failed to create the load-test room")?
        .error_for_status()
        .context("o-sfu rejected the load-test room")?;
    Ok(response
        .json::<RoomResponse>()
        .await
        .context("failed to decode the room response")?
        .uuid)
}

fn connect_token(room_id: &str, user_id: UserId) -> Result<String> {
    sign(
        &WebSocketConnectClaims {
            registered: RegisteredJwtClaims::default(),
            room_id: room_id.to_owned(),
            user_id,
            label: Some("load-peer".to_owned()),
            permissions: Some(UserPermissions::default()),
        },
        ROOM_KEY,
    )
    .context("failed to sign RTC peer claims")
}

async fn connect_receivers(
    websocket_url: &str,
    room_id: &str,
    receiver_count: u32,
) -> Result<Vec<ProtocolPeer>> {
    let capacity = usize::try_from(receiver_count).context("receiver count is too large")?;
    let mut receivers = Vec::with_capacity(capacity);
    for receiver_index in 0..receiver_count {
        let user_id = UserId::Integer(i64::from(receiver_index) + 2);
        let token = connect_token(room_id, user_id)?;
        receivers.push(Box::pin(ProtocolPeer::connect(websocket_url, &token, room_id)).await?);
    }
    Ok(receivers)
}

async fn warm_up_route(
    publisher: &mut ProtocolPeer,
    receivers: &mut [ProtocolPeer],
    source: &mut AudioSource,
) -> Result<()> {
    let mut route_ready = false;
    for _attempt in 0..WARMUP_ATTEMPTS {
        let expected_payload = publisher.send_audio_packet(source.next_packet()).await?;
        let observations = join_all(
            receivers
                .iter_mut()
                .map(|receiver| read_expected_payload(receiver, &expected_payload)),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        route_ready |= observations.into_iter().all(|observed| observed);
    }
    anyhow::ensure!(route_ready, "audio fanout route did not become ready");
    Ok(())
}

async fn read_expected_payload(
    receiver: &mut ProtocolPeer,
    expected_payload: &[u8],
) -> Result<bool> {
    let deadline = Instant::now() + WARMUP_TIMEOUT;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(payload) = receiver.read_rtp_payload(remaining).await? else {
            return Ok(false);
        };
        if payload == expected_payload {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn run_fixed_work(
    mut publisher: LoadPeer,
    receivers: Vec<LoadPeer>,
    mut source: AudioSource,
    spec: ScenarioSpec,
) -> Result<(Duration, String, Vec<String>, u64)> {
    let participant_count =
        usize::try_from(spec.receivers()).context("receiver count is too large")? + 1;
    let barrier = Arc::new(Barrier::new(participant_count));
    let (publisher_done, publisher_status) = watch::channel(false);
    let mut tasks = JoinSet::new();
    for (index, receiver) in receivers.into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let publisher_status = publisher_status.clone();
        let packet_count = spec.packets();
        tasks.spawn(receive_fixed_work(
            index,
            receiver,
            barrier,
            publisher_status,
            packet_count,
        ));
    }

    barrier.wait().await;
    let started_at = Instant::now();
    let mut sender_hasher = Sha256::new();
    for _packet_index in 0..spec.packets() {
        let payload = publisher.send_audio_packet(source.next_packet()).await?;
        sender_hasher.update(payload);
    }
    let sender_digest = finish_digest(sender_hasher)?;
    let publisher_completed_at = Instant::now();
    let _previous_status = publisher_done.send_replace(true);

    let mut receiver_results = Vec::new();
    let mut completed_receivers = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (receiver, result) = result.context("RTC receiver task failed")??;
        completed_receivers.push(receiver);
        receiver_results.push(result);
    }
    let completed_at = receiver_results
        .iter()
        .fold(publisher_completed_at, |latest, result| {
            latest.max(result.completed_at)
        });
    let elapsed = completed_at.duration_since(started_at);
    publisher.close().await?;
    for receiver in completed_receivers {
        receiver.close().await?;
    }
    receiver_results.sort_unstable_by_key(|result| result.index);
    let delivered_packets = receiver_results
        .iter()
        .map(|result| u64::from(result.packets))
        .sum();
    let receiver_digests = receiver_results
        .into_iter()
        .map(|result| result.digest)
        .collect();
    Ok((elapsed, sender_digest, receiver_digests, delivered_packets))
}

struct ReceiverResult {
    index: usize,
    packets: u32,
    digest: String,
    completed_at: Instant,
}

async fn receive_fixed_work(
    index: usize,
    mut receiver: LoadPeer,
    barrier: Arc<Barrier>,
    mut publisher_status: watch::Receiver<bool>,
    packet_count: u32,
) -> Result<(LoadPeer, ReceiverResult)> {
    barrier.wait().await;
    let mut hasher = Sha256::new();
    for _packet_index in 0..packet_count {
        let payload = receiver
            .read_rtp_payload(PACKET_TIMEOUT)
            .await?
            .context("timed out waiting for a fixed-work RTP packet")?;
        hasher.update(payload);
    }
    let completed_at = Instant::now();
    if !*publisher_status.borrow_and_update() {
        publisher_status
            .changed()
            .await
            .context("publisher stopped before fixed work completed")?;
    }
    anyhow::ensure!(
        receiver
            .read_rtp_payload(QUIESCENCE_TIMEOUT)
            .await?
            .is_none(),
        "received more than {packet_count} fixed-work RTP packets"
    );
    Ok((
        receiver,
        ReceiverResult {
            index,
            packets: packet_count,
            digest: finish_digest(hasher)?,
            completed_at,
        },
    ))
}

fn finish_digest(hasher: Sha256) -> Result<String> {
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut output, "{byte:02x}").context("failed to encode the payload digest")?;
    }
    Ok(output)
}

async fn write_result(output_path: &Path, result: &ScenarioResult) -> Result<()> {
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .await
            .context("failed to create the result directory")?;
    }
    let payload = serde_json::to_vec_pretty(result).context("failed to encode the result")?;
    fs::write(output_path, payload)
        .await
        .context("failed to write the scenario result")
}
