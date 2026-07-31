use std::{
    collections::VecDeque,
    io::ErrorKind,
    net::SocketAddr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use o_sfu_protocol::wire::NegotiationUploadSlot;
use o_sfu_rfc::webrtc;
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
    change::SdpOffer,
    format::{Codec, PayloadParams},
    media::{Direction, Media, MediaKind, Mid, Rid},
    net::{Protocol, Receive},
    rtp::{RtpWrite, Ssrc},
};
use tokio::{net::UdpSocket, time::timeout};

use super::media::{AudioPacket, VideoLayer, VideoPacket};

const RECEIVE_BUFFER_LEN: usize = 2_000;
const AUDIO_SSRC: u32 = 0x0f00_0001;
const VIDEO_LOW_SSRC: u32 = 0x0f00_0002;
const VIDEO_HIGH_SSRC: u32 = 0x0f00_0003;

pub struct RtcPeer {
    rtc: Rtc,
    socket: UdpSocket,
    local_addr: SocketAddr,
    connected: bool,
    audio_send_mid: Option<Mid>,
    video_upload_slot: Option<NegotiationUploadSlot>,
    declared_streams: DeclaredStreams,
    pending_rtp: VecDeque<Vec<u8>>,
    next_timeout: Instant,
    send_origin: Option<Instant>,
}

#[derive(Default)]
struct DeclaredStreams {
    audio: bool,
    video_low: bool,
    video_high: bool,
}

impl RtcPeer {
    pub async fn bind() -> Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .context("failed to bind RTC UDP socket")?;
        let local_addr = socket
            .local_addr()
            .context("failed to read RTC UDP socket address")?;
        let now = Instant::now();
        let mut rtc = Rtc::builder().set_rtp_mode(true).build(now);
        let candidate =
            Candidate::host(local_addr, "udp").context("failed to build RTC host candidate")?;
        ensure!(
            rtc.add_local_candidate(candidate).is_some(),
            "RTC rejected its host candidate"
        );
        let mut peer = Self {
            rtc,
            socket,
            local_addr,
            connected: false,
            audio_send_mid: None,
            video_upload_slot: None,
            declared_streams: DeclaredStreams::default(),
            pending_rtp: VecDeque::new(),
            next_timeout: now,
            send_origin: None,
        };
        peer.drain_output().await?;
        Ok(peer)
    }

    pub async fn answer_offer(
        &mut self,
        offer_sdp: &str,
        upload_slots: &[NegotiationUploadSlot],
    ) -> Result<String> {
        let offer =
            SdpOffer::from_sdp_string(offer_sdp).context("failed to parse the SFU SDP offer")?;
        if let Some(slot) = upload_slots.iter().find(|slot| {
            slot.kind == webrtc::MediaKind::Video && slot.codecs.iter().any(|codec| codec == "VP8")
        }) {
            if self
                .video_upload_slot
                .as_ref()
                .is_some_and(|current| current.mid != slot.mid)
            {
                self.declared_streams.video_low = false;
                self.declared_streams.video_high = false;
            }
            self.video_upload_slot = Some(slot.clone());
        }
        let video_slot = self.video_upload_slot.clone();
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .context("failed to accept the SFU SDP offer")?;
        self.drain_output().await?;
        let answer_sdp = answer.to_sdp_string();
        match video_slot.as_ref() {
            Some(slot) => answer_with_simulcast_send_rids(&answer_sdp, slot),
            None => Ok(answer_sdp),
        }
    }

    pub async fn wait_until_connected(&mut self, timeout_window: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout_window;
        while !self.connected && Instant::now() < deadline {
            self.wait_for_input(deadline).await?;
        }
        ensure!(self.connected, "RTC connection did not become ready");
        Ok(())
    }

    pub async fn send_audio_packet(&mut self, packet: AudioPacket) -> Result<usize> {
        let packet_wallclock = self.pace_until(packet.emitted_at).await?;
        let mid = self
            .audio_send_mid
            .context("audio publication has no negotiated send path")?;
        let payload_type = self
            .rtc
            .codec_config()
            .find(|params| params.spec().codec == Codec::Opus)
            .map(PayloadParams::pt)
            .context("Opus was not negotiated")?;
        if !self.declared_streams.audio {
            let _stream =
                self.rtc
                    .direct_api()
                    .declare_stream_tx(Ssrc::from(AUDIO_SSRC), None, mid, None);
            self.drain_output().await?;
            self.declared_streams.audio = true;
        }
        let payload_len = packet.payload.len();
        {
            let mut direct_api = self.rtc.direct_api();
            let stream = direct_api
                .stream_tx_by_mid(mid, None)
                .context("failed to find the audio RTP stream")?;
            stream.write_rtp(
                RtpWrite::new(
                    payload_type,
                    u64::from(packet.sequence_number).into(),
                    packet.rtp_timestamp,
                    packet_wallclock,
                    packet.payload,
                )
                .ext_vals(packet.extension_values),
            );
        }
        self.drain_output().await?;
        self.apply_due_timeouts().await?;
        Ok(payload_len)
    }

    pub async fn send_video_packet(&mut self, packet: VideoPacket) -> Result<usize> {
        let packet_wallclock = self.pace_until(packet.emitted_at).await?;
        let mid = self
            .video_upload_slot
            .as_ref()
            .map(|slot| Mid::from(slot.mid.as_str()))
            .context("camera publication has no negotiated send path")?;
        let payload_type = self
            .rtc
            .codec_config()
            .find(|params| params.spec().codec == Codec::Vp8)
            .map(PayloadParams::pt)
            .context("VP8 was not negotiated")?;
        self.ensure_video_stream(mid, packet.layer).await?;
        let payload_len = packet.payload.len();
        let rid = Rid::from(packet.layer.rid());
        let mut extension_values = packet.extension_values;
        extension_values.mid = Some(mid);
        extension_values.rid = Some(rid);
        {
            let mut direct_api = self.rtc.direct_api();
            let stream = direct_api
                .stream_tx_by_mid(mid, Some(rid))
                .context("failed to find the video RTP stream")?;
            stream.write_rtp(
                RtpWrite::new(
                    payload_type,
                    u64::from(packet.sequence_number).into(),
                    packet.rtp_timestamp,
                    packet_wallclock,
                    packet.payload,
                )
                .marker(packet.marker)
                .ext_vals(extension_values),
            );
        }
        self.drain_output().await?;
        self.apply_due_timeouts().await?;
        Ok(payload_len)
    }

    pub async fn read_rtp_payload(&mut self, timeout_window: Duration) -> Result<Option<Vec<u8>>> {
        if let Some(payload) = self.pending_rtp.pop_front() {
            return Ok(Some(payload));
        }
        if timeout_window.is_zero() {
            while self.try_read_input().await? {
                if let Some(payload) = self.pending_rtp.pop_front() {
                    return Ok(Some(payload));
                }
            }
            return Ok(None);
        }
        let deadline = Instant::now() + timeout_window;
        while Instant::now() < deadline {
            self.wait_for_input(deadline).await?;
            if let Some(payload) = self.pending_rtp.pop_front() {
                return Ok(Some(payload));
            }
        }
        Ok(None)
    }

    pub(super) fn reset_send_pacing(&mut self) {
        self.send_origin = None;
    }

    async fn pace_until(&mut self, emitted_at: Duration) -> Result<Instant> {
        let send_origin = *self.send_origin.get_or_insert_with(|| {
            Instant::now()
                .checked_sub(emitted_at)
                .unwrap_or_else(Instant::now)
        });
        let send_at = send_origin + emitted_at;
        while Instant::now() < send_at {
            self.wait_for_input(send_at).await?;
        }
        Ok(send_at)
    }

    async fn ensure_video_stream(&mut self, mid: Mid, layer: VideoLayer) -> Result<()> {
        let stream_declared = match layer {
            VideoLayer::Low => self.declared_streams.video_low,
            VideoLayer::High => self.declared_streams.video_high,
        };
        if stream_declared {
            return Ok(());
        }
        let ssrc = match layer {
            VideoLayer::Low => VIDEO_LOW_SSRC,
            VideoLayer::High => VIDEO_HIGH_SSRC,
        };
        let _stream = self.rtc.direct_api().declare_stream_tx(
            Ssrc::from(ssrc),
            None,
            mid,
            Some(Rid::from(layer.rid())),
        );
        self.drain_output().await?;
        match layer {
            VideoLayer::Low => self.declared_streams.video_low = true,
            VideoLayer::High => self.declared_streams.video_high = true,
        }
        Ok(())
    }

    async fn drain_output(&mut self) -> Result<()> {
        loop {
            match self
                .rtc
                .poll_output()
                .context("failed to drain RTC output")?
            {
                Output::Transmit(transmit) => {
                    self.socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await
                        .context("failed to send an RTC datagram")?;
                }
                Output::Event(event) => self.observe_event(event)?,
                Output::Timeout(timeout_at) => {
                    self.next_timeout = timeout_at;
                    return Ok(());
                }
            }
        }
    }

    async fn apply_due_timeouts(&mut self) -> Result<()> {
        while self.next_timeout <= Instant::now() {
            self.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .context("failed to apply a due RTC timeout")?;
            self.drain_output().await?;
        }
        Ok(())
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "str0m marks Event non-exhaustive so future diagnostic events must be ignored"
    )]
    fn observe_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Connected => {
                self.connected = true;
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                return Err(anyhow!("RTC connection disconnected"));
            }
            Event::MediaAdded(media) => {
                self.observe_audio_direction(media.mid, media.kind, media.direction);
            }
            Event::MediaChanged(media) => {
                let kind = self.rtc.media(media.mid).map(Media::kind);
                if let Some(kind) = kind {
                    self.observe_audio_direction(media.mid, kind, media.direction);
                }
            }
            Event::RtpPacket(packet) => {
                self.pending_rtp.push_back(packet.payload.as_ref().to_vec());
            }
            _ => {}
        }
        Ok(())
    }

    fn observe_audio_direction(&mut self, mid: Mid, kind: MediaKind, direction: Direction) {
        if kind != MediaKind::Audio {
            return;
        }
        if direction.is_sending() {
            if self.audio_send_mid != Some(mid) {
                self.declared_streams.audio = false;
            }
            self.audio_send_mid = Some(mid);
        } else if self.audio_send_mid == Some(mid) {
            self.audio_send_mid = None;
            self.declared_streams.audio = false;
        }
    }

    async fn wait_for_input(&mut self, deadline: Instant) -> Result<()> {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if self.next_timeout <= now {
            self.rtc
                .handle_input(Input::Timeout(now))
                .context("failed to apply an RTC timeout")?;
            return self.drain_output().await;
        }
        let wait_until = self.next_timeout.min(deadline);
        let wait_duration = wait_until.saturating_duration_since(now);
        let mut receive_buffer = [0_u8; RECEIVE_BUFFER_LEN];
        match timeout(wait_duration, self.socket.recv_from(&mut receive_buffer)).await {
            Ok(Ok((received_size, source_addr))) if received_size > 0 => {
                self.apply_datagram(&receive_buffer, received_size, source_addr)
                    .await?;
            }
            Ok(Ok((_received_size, _source_addr))) => {}
            Ok(Err(error)) => return Err(error).context("failed to receive an RTC datagram"),
            Err(_elapsed) => {
                let now = Instant::now();
                if self.next_timeout <= now {
                    self.rtc
                        .handle_input(Input::Timeout(now))
                        .context("failed to apply an RTC timeout")?;
                    self.drain_output().await?;
                }
            }
        }
        Ok(())
    }

    async fn try_read_input(&mut self) -> Result<bool> {
        self.apply_due_timeouts().await?;
        let mut receive_buffer = [0_u8; RECEIVE_BUFFER_LEN];
        match self.socket.try_recv_from(&mut receive_buffer) {
            Ok((received_size, source_addr)) if received_size > 0 => {
                self.apply_datagram(&receive_buffer, received_size, source_addr)
                    .await?;
                Ok(true)
            }
            Ok((_received_size, _source_addr)) => Ok(true),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error).context("failed to receive an RTC datagram"),
        }
    }

    async fn apply_datagram(
        &mut self,
        receive_buffer: &[u8],
        received_size: usize,
        source_addr: SocketAddr,
    ) -> Result<()> {
        let packet = receive_buffer
            .get(..received_size)
            .context("RTC datagram exceeded its receive buffer")?;
        let receive = Receive {
            proto: Protocol::Udp,
            source: source_addr,
            destination: self.local_addr,
            contents: packet
                .try_into()
                .map_err(|_error| anyhow!("failed to borrow the RTC datagram"))?,
        };
        self.rtc
            .handle_input(Input::Receive(Instant::now(), receive))
            .context("failed to apply an RTC datagram")?;
        self.drain_output().await
    }
}

fn answer_with_simulcast_send_rids(
    answer_sdp: &str,
    slot: &NegotiationUploadSlot,
) -> Result<String> {
    ensure!(
        !slot.simulcast_encodings.is_empty(),
        "VP8 upload slot has no simulcast encodings"
    );
    let marker = format!("a=mid:{}\r\n", slot.mid);
    ensure!(answer_sdp.contains(&marker), "VP8 answer has no upload MID");
    let mut replacement = marker.clone();
    for encoding in &slot.simulcast_encodings {
        replacement.push_str("a=rid:");
        replacement.push_str(&encoding.rid);
        replacement.push_str(" send");
        if let Some(max_bitrate) = encoding.max_bitrate {
            replacement.push_str(" max-br=");
            replacement.push_str(&max_bitrate.to_string());
        }
        replacement.push_str("\r\n");
    }
    replacement.push_str("a=simulcast:send ");
    for (index, encoding) in slot.simulcast_encodings.iter().enumerate() {
        if index > 0 {
            replacement.push(';');
        }
        replacement.push_str(&encoding.rid);
    }
    replacement.push_str("\r\n");
    Ok(answer_sdp.replacen(&marker, &replacement, 1))
}
