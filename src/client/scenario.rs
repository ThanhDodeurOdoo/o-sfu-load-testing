use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::future::join_all;
use o_sfu::{
    auth::{HttpRoomClaims, RegisteredJwtClaims, WebSocketConnectClaims, sign},
    http::{CreateRoomQuery, RoomResponse, route},
};
use o_sfu_protocol::wire::{UserId, UserPermissions, VideoLayoutIntent};
use tokio::{
    fs,
    sync::Barrier,
    task::JoinSet,
    time::{Instant, timeout},
};

use crate::{
    AUDIO_PACKETS_PER_SECOND, AUTH_KEY, CorrectnessSummary, ROOM_ISSUER, ROOM_KEY, RunObservation,
    ScenarioResult, ScenarioSpec, VIDEO_FRAMES_PER_SECOND,
    client::{
        media::{
            AUDIO_FRAME_INTERVAL, AudioSource, PacketPhase, PayloadKind, VIDEO_FRAME_INTERVAL,
            VideoLayer, VideoSource, inspect_payload,
        },
        protocol::{LoadPeer, ProtocolPeer},
    },
    video_packets_per_layer,
};

const PACKET_TIMEOUT: Duration = Duration::from_secs(10);
const QUIESCENCE_TIMEOUT: Duration = Duration::from_millis(200);
const ROUTE_SETTLE_TIME: Duration = Duration::from_millis(300);
const WARMUP_AUDIO_PACKETS: u32 = 5;
const WARMUP_VIDEO_FRAMES: u32 = 2;

#[derive(Clone)]
struct ScenarioSync {
    barrier: Arc<Barrier>,
    measured_at: Arc<OnceLock<Instant>>,
}

/// Runs one exact fixed-work scenario through public production boundaries.
///
/// # Errors
///
/// Returns an error when room setup, signaling, RTC transport, result
/// persistence or exact packet accounting fails.
pub async fn run(
    base_url: &str,
    websocket_url: &str,
    output_path: &Path,
    spec: ScenarioSpec,
) -> Result<ScenarioResult> {
    spec.validate()?;
    let peer_total = u64::from(spec.room_count())
        .checked_mul(u64::from(spec.peers_per_room()))
        .context("scenario peer count overflowed")?;
    let sync = ScenarioSync {
        barrier: Arc::new(Barrier::new(
            usize::try_from(peer_total).context("scenario peer count exceeds usize")?,
        )),
        measured_at: Arc::new(OnceLock::new()),
    };
    let mut room_tasks = JoinSet::new();
    for room_index in 0..spec.room_count() {
        let base_url = base_url.to_owned();
        let websocket_url = websocket_url.to_owned();
        let sync = sync.clone();
        room_tasks.spawn(async move {
            let room_id = create_room(&base_url, room_index).await?;
            match spec {
                ScenarioSpec::Smoke { receivers, packets } => {
                    run_audio_room(
                        &websocket_url,
                        &room_id,
                        receivers + 1,
                        1,
                        packets,
                        room_index,
                        sync,
                    )
                    .await
                }
                ScenarioSpec::AudioMesh { peers, seconds, .. } => {
                    let packets = seconds
                        .checked_mul(AUDIO_PACKETS_PER_SECOND)
                        .context("audio packet count exceeds u32")?;
                    run_audio_room(
                        &websocket_url,
                        &room_id,
                        peers,
                        peers,
                        packets,
                        room_index,
                        sync,
                    )
                    .await
                }
                ScenarioSpec::VideoGallery {
                    peers,
                    publishers,
                    seconds,
                    ..
                } => {
                    run_video_room(
                        &websocket_url,
                        &room_id,
                        peers,
                        publishers,
                        seconds,
                        room_index,
                        sync,
                    )
                    .await
                }
            }
            .with_context(|| format!("load room {room_index} failed"))
        });
    }

    let mut observation = RunObservation::default();
    while let Some(result) = room_tasks.join_next().await {
        observation.merge(result.context("load room task failed")??);
    }
    let result = ScenarioResult::completed(spec, observation)?;
    write_result(output_path, &result).await?;
    result.validate(spec)?;
    Ok(result)
}

async fn run_audio_room(
    websocket_url: &str,
    room_id: &str,
    peer_count: u32,
    publisher_count: u32,
    packet_count: u32,
    room_index: u32,
    sync: ScenarioSync,
) -> Result<RunObservation> {
    let mut peers = connect_peers(websocket_url, room_id, peer_count).await?;
    publish_sources(&mut peers, publisher_count, PublishedKind::Audio).await?;
    let peers = peers
        .into_iter()
        .map(ProtocolPeer::into_load_peer)
        .collect::<Result<Vec<_>>>()?;
    let publisher_count_usize =
        usize::try_from(publisher_count).context("publisher count exceeds usize")?;
    let mut tasks = JoinSet::new();
    for (peer_index, peer) in peers.into_iter().enumerate() {
        let peer_source = source_id(room_index, peer_count, peer_index)?;
        let source = (peer_index < publisher_count_usize).then(|| AudioSource::new(peer_source));
        let mut expected = Vec::with_capacity(publisher_count_usize.saturating_sub(1));
        for source_index in 0..publisher_count {
            let source_index_usize =
                usize::try_from(source_index).context("audio source index exceeds usize")?;
            if source_index_usize != peer_index {
                expected.push(ExpectedStream::audio(
                    source_id(room_index, peer_count, source_index_usize)?,
                    packet_count,
                )?);
            }
        }
        tasks.spawn(run_audio_peer(
            peer,
            source,
            PacketLedger::new(expected),
            packet_count,
            sync.clone(),
        ));
    }
    collect_peer_observations(tasks).await
}

async fn run_video_room(
    websocket_url: &str,
    room_id: &str,
    peer_count: u32,
    publisher_count: u32,
    seconds: u32,
    room_index: u32,
    sync: ScenarioSync,
) -> Result<RunObservation> {
    let mut peers = connect_peers(websocket_url, room_id, peer_count).await?;
    configure_video_layouts(&mut peers, publisher_count).await?;
    publish_sources(&mut peers, publisher_count, PublishedKind::Camera).await?;
    let peers = peers
        .into_iter()
        .map(ProtocolPeer::into_load_peer)
        .collect::<Result<Vec<_>>>()?;
    let (low_packets, high_packets) = video_packets_per_layer(seconds)?;
    let frame_count = seconds
        .checked_mul(VIDEO_FRAMES_PER_SECOND)
        .context("video frame count exceeds u32")?;
    let mut tasks = JoinSet::new();
    let publisher_count_usize =
        usize::try_from(publisher_count).context("publisher count exceeds usize")?;
    for (peer_index, peer) in peers.into_iter().enumerate() {
        let peer_source = source_id(room_index, peer_count, peer_index)?;
        let source = (peer_index < publisher_count_usize).then(|| VideoSource::new(peer_source));
        let featured = featured_source(peer_index, publisher_count);
        let mut expected = Vec::new();
        for source_index in 0..publisher_count {
            let source_index_usize =
                usize::try_from(source_index).context("video source index exceeds usize")?;
            if source_index_usize == peer_index {
                continue;
            }
            let layer = if featured == Some(source_index) {
                VideoLayer::High
            } else {
                VideoLayer::Low
            };
            let packet_count = match layer {
                VideoLayer::Low => low_packets,
                VideoLayer::High => high_packets,
            };
            expected.push(ExpectedStream::video(
                source_id(room_index, peer_count, source_index_usize)?,
                layer,
                u32::try_from(packet_count).context("video packet count exceeds u32")?,
            )?);
        }
        tasks.spawn(run_video_peer(
            peer,
            source,
            PacketLedger::new(expected),
            frame_count,
            sync.clone(),
        ));
    }
    collect_peer_observations(tasks).await
}

async fn run_audio_peer(
    mut peer: LoadPeer,
    mut source: Option<AudioSource>,
    mut ledger: PacketLedger,
    packet_count: u32,
    sync: ScenarioSync,
) -> Result<RunObservation> {
    sync.barrier.wait().await;
    if let Some(source) = source.as_mut() {
        for ordinal in 0..WARMUP_AUDIO_PACKETS {
            let _payload_len = peer
                .send_audio_packet(source.next_packet(PacketPhase::Warmup, ordinal))
                .await?;
            drain_pending(&mut peer, &mut ledger).await?;
        }
    }
    drain_for(&mut peer, &mut ledger, ROUTE_SETTLE_TIME).await?;
    sync.barrier.wait().await;
    if let Some(source) = source.as_mut() {
        source.reset_timeline();
    }
    peer.reset_send_pacing();
    sync.barrier.wait().await;
    let started_at = *sync.measured_at.get_or_init(Instant::now);
    let target_duration = AUDIO_FRAME_INTERVAL.saturating_mul(packet_count);
    let deadline = started_at + target_duration + PACKET_TIMEOUT;
    let mut observation = RunObservation::default();
    if let Some(source) = source.as_mut() {
        for ordinal in 0..packet_count {
            let packet = source.next_packet(PacketPhase::Measured, ordinal);
            let emitted_at = packet.emitted_at;
            let payload_len = peer.send_audio_packet(packet).await?;
            observation.offered_packets = observation.offered_packets.saturating_add(1);
            observation.offered_payload_bytes = observation
                .offered_payload_bytes
                .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
            observation.max_send_lag_ms = observation.max_send_lag_ms.max(elapsed_millis(
                Instant::now().saturating_duration_since(started_at + emitted_at),
            ));
            drain_pending(&mut peer, &mut ledger).await?;
        }
    }
    receive_until_complete(&mut peer, &mut ledger, deadline).await?;
    let completed_at = Instant::now();
    sync.barrier.wait().await;
    drain_for(&mut peer, &mut ledger, QUIESCENCE_TIMEOUT).await?;
    observation.elapsed_ms =
        elapsed_millis_ceil(completed_at.duration_since(started_at).max(target_duration)).max(1);
    observation.delivered_packets = ledger.delivered_packets;
    observation.delivered_payload_bytes = ledger.delivered_payload_bytes;
    observation.correctness = ledger.finish();
    peer.close().await?;
    Ok(observation)
}

async fn run_video_peer(
    mut peer: LoadPeer,
    mut source: Option<VideoSource>,
    mut ledger: PacketLedger,
    frame_count: u32,
    sync: ScenarioSync,
) -> Result<RunObservation> {
    sync.barrier.wait().await;
    if let Some(source) = source.as_mut() {
        for frame in 0..WARMUP_VIDEO_FRAMES {
            for packet in source.next_frame(PacketPhase::Warmup, frame) {
                let _payload_len = peer.send_video_packet(packet).await?;
                drain_pending(&mut peer, &mut ledger).await?;
            }
        }
    }
    drain_for(&mut peer, &mut ledger, ROUTE_SETTLE_TIME).await?;
    sync.barrier.wait().await;
    if let Some(source) = source.as_mut() {
        source.reset_timeline();
    }
    peer.reset_send_pacing();
    sync.barrier.wait().await;
    let started_at = *sync.measured_at.get_or_init(Instant::now);
    let target_duration = VIDEO_FRAME_INTERVAL.saturating_mul(frame_count);
    let deadline = started_at + target_duration + PACKET_TIMEOUT;
    let mut observation = RunObservation::default();
    if let Some(source) = source.as_mut() {
        for frame in 0..frame_count {
            for packet in source.next_frame(PacketPhase::Measured, frame) {
                let emitted_at = packet.emitted_at;
                let payload_len = peer.send_video_packet(packet).await?;
                observation.offered_packets = observation.offered_packets.saturating_add(1);
                observation.offered_payload_bytes = observation
                    .offered_payload_bytes
                    .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
                observation.max_send_lag_ms = observation.max_send_lag_ms.max(elapsed_millis(
                    Instant::now().saturating_duration_since(started_at + emitted_at),
                ));
                drain_pending(&mut peer, &mut ledger).await?;
            }
        }
    }
    receive_until_complete(&mut peer, &mut ledger, deadline).await?;
    let completed_at = Instant::now();
    sync.barrier.wait().await;
    drain_for(&mut peer, &mut ledger, QUIESCENCE_TIMEOUT).await?;
    observation.elapsed_ms =
        elapsed_millis_ceil(completed_at.duration_since(started_at).max(target_duration)).max(1);
    observation.delivered_packets = ledger.delivered_packets;
    observation.delivered_payload_bytes = ledger.delivered_payload_bytes;
    observation.correctness = ledger.finish();
    peer.close().await?;
    Ok(observation)
}

async fn collect_peer_observations(
    mut tasks: JoinSet<Result<RunObservation>>,
) -> Result<RunObservation> {
    let mut observation = RunObservation::default();
    while let Some(result) = tasks.join_next().await {
        observation.merge(result.context("RTC peer task failed")??);
    }
    Ok(observation)
}

async fn connect_peers(
    websocket_url: &str,
    room_id: &str,
    peer_count: u32,
) -> Result<Vec<ProtocolPeer>> {
    let mut tokens =
        Vec::with_capacity(usize::try_from(peer_count).context("peer count exceeds usize")?);
    for peer_index in 0..peer_count {
        tokens.push(connect_token(
            room_id,
            UserId::Integer(i64::from(peer_index) + 1),
        )?);
    }
    join_all(
        tokens
            .iter()
            .map(|token| ProtocolPeer::connect(websocket_url, token, room_id)),
    )
    .await
    .into_iter()
    .collect()
}

#[derive(Clone, Copy)]
enum PublishedKind {
    Audio,
    Camera,
}

async fn publish_sources(
    peers: &mut [ProtocolPeer],
    publisher_count: u32,
    kind: PublishedKind,
) -> Result<()> {
    for publisher_index in 0..publisher_count {
        let index = usize::try_from(publisher_index).context("publisher index exceeds usize")?;
        let publisher = peers.get_mut(index).context("publisher is missing")?;
        match kind {
            PublishedKind::Audio => publisher.publish_audio().await?,
            PublishedKind::Camera => publisher.publish_camera().await?,
        }
        let negotiations = peers
            .iter_mut()
            .enumerate()
            .filter_map(|(peer_index, peer)| {
                (peer_index != index).then_some(peer.accept_next_negotiation())
            });
        join_all(negotiations)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
    }
    Ok(())
}

async fn configure_video_layouts(peers: &mut [ProtocolPeer], publisher_count: u32) -> Result<()> {
    for (peer_index, peer) in peers.iter_mut().enumerate() {
        let featured = featured_source(peer_index, publisher_count);
        let layouts = (0..publisher_count).filter_map(|source_index| {
            (usize::try_from(source_index).ok() != Some(peer_index)).then_some((
                UserId::Integer(i64::from(source_index) + 1),
                if featured == Some(source_index) {
                    VideoLayoutIntent::Featured
                } else {
                    VideoLayoutIntent::VisibleThumbnail
                },
            ))
        });
        peer.set_camera_layouts(layouts).await?;
    }
    Ok(())
}

fn featured_source(peer_index: usize, publisher_count: u32) -> Option<u32> {
    let publisher_count = usize::try_from(publisher_count).ok()?;
    if publisher_count == 0 {
        return None;
    }
    let candidate = peer_index % publisher_count;
    let featured = if candidate == peer_index {
        (publisher_count > 1).then_some((candidate + 1) % publisher_count)?
    } else {
        candidate
    };
    u32::try_from(featured).ok()
}

fn source_id(room_index: u32, peers_per_room: u32, peer_index: usize) -> Result<u16> {
    let peer_index = u32::try_from(peer_index).context("peer index exceeds u32")?;
    let source = room_index
        .checked_mul(peers_per_room)
        .and_then(|offset| offset.checked_add(peer_index))
        .context("global source index overflowed")?;
    u16::try_from(source).context("global source index exceeds u16")
}

struct ExpectedStream {
    source: u16,
    kind: PayloadKind,
    layer: Option<VideoLayer>,
    seen: Vec<bool>,
    next_ordinal: usize,
}

impl ExpectedStream {
    fn audio(source: u16, packets: u32) -> Result<Self> {
        Self::new(source, PayloadKind::Audio, None, packets)
    }

    fn video(source: u16, layer: VideoLayer, packets: u32) -> Result<Self> {
        Self::new(source, PayloadKind::Video, Some(layer), packets)
    }

    fn new(
        source: u16,
        kind: PayloadKind,
        layer: Option<VideoLayer>,
        packets: u32,
    ) -> Result<Self> {
        ensure!(packets > 0, "expected stream has no packets");
        Ok(Self {
            source,
            kind,
            layer,
            seen: vec![false; usize::try_from(packets).context("packet count exceeds usize")?],
            next_ordinal: 0,
        })
    }

    fn is_complete(&self) -> bool {
        self.next_ordinal == self.seen.len()
    }
}

struct PacketLedger {
    streams: Vec<ExpectedStream>,
    delivered_packets: u64,
    delivered_payload_bytes: u64,
    correctness: CorrectnessSummary,
}

impl PacketLedger {
    const fn new(streams: Vec<ExpectedStream>) -> Self {
        Self {
            streams,
            delivered_packets: 0,
            delivered_payload_bytes: 0,
            correctness: CorrectnessSummary {
                missing_packets: 0,
                duplicate_packets: 0,
                out_of_order_packets: 0,
                unexpected_packets: 0,
                payload_mismatches: 0,
            },
        }
    }

    fn observe(&mut self, payload: &[u8]) {
        let Ok(inspection) = inspect_payload(payload) else {
            self.correctness.unexpected_packets =
                self.correctness.unexpected_packets.saturating_add(1);
            return;
        };
        if inspection.identity.phase == PacketPhase::Warmup {
            return;
        }
        let identity = inspection.identity;
        let Some(stream) = self.streams.iter_mut().find(|stream| {
            stream.source == identity.source
                && stream.kind == identity.kind
                && stream.layer == identity.layer
        }) else {
            self.correctness.unexpected_packets =
                self.correctness.unexpected_packets.saturating_add(1);
            return;
        };
        let Ok(ordinal) = usize::try_from(identity.ordinal) else {
            self.correctness.unexpected_packets =
                self.correctness.unexpected_packets.saturating_add(1);
            return;
        };
        let Some(seen) = stream.seen.get_mut(ordinal) else {
            self.correctness.unexpected_packets =
                self.correctness.unexpected_packets.saturating_add(1);
            return;
        };
        if *seen {
            self.correctness.duplicate_packets =
                self.correctness.duplicate_packets.saturating_add(1);
            return;
        }
        if !inspection.payload_matches {
            self.correctness.payload_mismatches =
                self.correctness.payload_mismatches.saturating_add(1);
        }
        if ordinal != stream.next_ordinal {
            self.correctness.out_of_order_packets =
                self.correctness.out_of_order_packets.saturating_add(1);
        }
        *seen = true;
        self.delivered_packets = self.delivered_packets.saturating_add(1);
        self.delivered_payload_bytes = self
            .delivered_payload_bytes
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        while stream.seen.get(stream.next_ordinal) == Some(&true) {
            stream.next_ordinal = stream.next_ordinal.saturating_add(1);
        }
    }

    fn is_complete(&self) -> bool {
        self.streams.iter().all(ExpectedStream::is_complete)
    }

    fn finish(mut self) -> CorrectnessSummary {
        self.correctness.missing_packets = self
            .streams
            .iter()
            .map(|stream| stream.seen.iter().filter(|seen| !**seen).count())
            .map(|missing| u64::try_from(missing).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add);
        self.correctness
    }
}

async fn receive_until_complete(
    peer: &mut LoadPeer,
    ledger: &mut PacketLedger,
    deadline: Instant,
) -> Result<()> {
    while !ledger.is_complete() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Some(payload) = peer.read_rtp_payload(remaining).await? {
            ledger.observe(&payload);
        }
    }
    Ok(())
}

async fn drain_pending(peer: &mut LoadPeer, ledger: &mut PacketLedger) -> Result<()> {
    while let Some(payload) = peer.read_rtp_payload(Duration::ZERO).await? {
        ledger.observe(&payload);
    }
    Ok(())
}

async fn drain_for(
    peer: &mut LoadPeer,
    ledger: &mut PacketLedger,
    duration: Duration,
) -> Result<()> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, peer.read_rtp_payload(remaining)).await {
            Ok(result) => {
                if let Some(payload) = result? {
                    ledger.observe(&payload);
                }
            }
            Err(_elapsed) => break,
        }
    }
    Ok(())
}

async fn create_room(base_url: &str, room_index: u32) -> Result<String> {
    let token = sign(
        &HttpRoomClaims {
            registered: RegisteredJwtClaims {
                iss: Some(format!("{ROOM_ISSUER}-{room_index}")),
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

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_millis_ceil(duration: Duration) -> u64 {
    let milliseconds = duration.as_nanos().div_ceil(1_000_000);
    u64::try_from(milliseconds).unwrap_or(u64::MAX)
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

#[cfg(test)]
#[path = "../TESTS/scenario_tests.rs"]
mod tests;
