use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use o_sfu_protocol::{
    host::{Command, CommandBatch, ProtocolCore},
    wire::{DownloadStates, StreamType, UserId, VideoLayoutIntent},
};
use tokio::{
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use super::{
    media::{AudioPacket, VideoPacket},
    rtc::RtcPeer,
};

type TestWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct ProtocolPeer {
    core: ProtocolCore,
    websocket: Option<TestWebSocket>,
    rtc: Option<RtcPeer>,
    timers: BTreeMap<u32, u32>,
    negotiation_count: u64,
}

pub struct LoadPeer {
    rtc: RtcPeer,
    signaling_shutdown: Option<oneshot::Sender<()>>,
    signaling_task: Option<JoinHandle<Result<()>>>,
}

impl ProtocolPeer {
    pub async fn connect(url: &str, jwt: &str, room_id: &str) -> Result<Self> {
        let mut peer = Self {
            core: ProtocolCore::new(),
            websocket: None,
            rtc: None,
            timers: BTreeMap::new(),
            negotiation_count: 0,
        };
        let commands = peer
            .core
            .connect(url.to_owned(), jwt.to_owned(), Some(room_id.to_owned()));
        Box::pin(peer.run_commands(commands)).await?;
        Box::pin(peer.read_until_negotiation(1)).await?;
        peer.rtc_mut()?
            .wait_until_connected(Duration::from_secs(10))
            .await?;
        let commands = peer.core.on_transport_ready();
        Box::pin(peer.run_commands(commands)).await?;
        Ok(peer)
    }

    pub async fn publish_audio(&mut self) -> Result<()> {
        Box::pin(self.publish(StreamType::Audio)).await
    }

    pub async fn publish_camera(&mut self) -> Result<()> {
        Box::pin(self.publish(StreamType::Camera)).await
    }

    pub async fn set_camera_layouts(
        &mut self,
        layouts: impl IntoIterator<Item = (UserId, VideoLayoutIntent)>,
    ) -> Result<()> {
        let mut changed = false;
        for (user_id, camera_layout) in layouts {
            changed = true;
            let commands = self.core.subscribe(
                user_id,
                DownloadStates {
                    camera: Some(!matches!(camera_layout, VideoLayoutIntent::Hidden)),
                    camera_layout: Some(camera_layout),
                    ..DownloadStates::default()
                },
            );
            Box::pin(self.run_commands(commands)).await?;
        }
        if changed {
            Box::pin(self.flush_next_timer()).await?;
        }
        Ok(())
    }

    async fn publish(&mut self, stream_type: StreamType) -> Result<()> {
        let commands = self.core.publish(stream_type, true);
        Box::pin(self.run_commands(commands)).await?;
        Box::pin(self.flush_next_timer()).await?;
        let expected_negotiations = self.negotiation_count + 1;
        Box::pin(self.read_until_negotiation(expected_negotiations)).await
    }

    pub async fn accept_next_negotiation(&mut self) -> Result<()> {
        let expected_negotiations = self.negotiation_count + 1;
        Box::pin(self.read_until_negotiation(expected_negotiations)).await
    }

    pub fn into_load_peer(mut self) -> Result<LoadPeer> {
        let websocket = self
            .websocket
            .take()
            .context("protocol WebSocket is not connected")?;
        let mut rtc = self
            .rtc
            .take()
            .context("protocol RTC peer is not available")?;
        rtc.reset_send_pacing();
        let (signaling_shutdown, shutdown_receiver) = oneshot::channel();
        let signaling_task = tokio::spawn(service_signaling(websocket, shutdown_receiver));
        Ok(LoadPeer {
            rtc,
            signaling_shutdown: Some(signaling_shutdown),
            signaling_task: Some(signaling_task),
        })
    }

    async fn read_until_negotiation(&mut self, expected_count: u64) -> Result<()> {
        while self.negotiation_count < expected_count {
            let frame = timeout(
                Duration::from_secs(10),
                read_text_message(self.websocket_mut()?),
            )
            .await
            .context("timed out waiting for an o-sfu signaling frame")??;
            let commands = self.core.on_ws_message(&frame);
            Box::pin(self.run_commands(commands)).await?;
        }
        Ok(())
    }

    async fn flush_next_timer(&mut self) -> Result<()> {
        let (timer_id, delay_ms) = self
            .timers
            .iter()
            .min_by_key(|(_id, delay_ms)| **delay_ms)
            .map(|(id, delay_ms)| (*id, *delay_ms))
            .context("protocol did not schedule its outbound batch")?;
        sleep(Duration::from_millis(u64::from(delay_ms))).await;
        let _ = self.timers.remove(&timer_id);
        let commands = self.core.on_timer(timer_id);
        Box::pin(self.run_commands(commands)).await
    }

    async fn run_commands(&mut self, commands: CommandBatch) -> Result<()> {
        let mut pending: VecDeque<_> = commands.into_vec().into();
        while let Some(command) = pending.pop_front() {
            match command {
                Command::Connect { url } => {
                    let (websocket, _response) = connect_async(url)
                        .await
                        .context("failed to connect to the o-sfu WebSocket")?;
                    self.websocket = Some(websocket);
                    pending.extend(self.core.on_ws_open().into_vec());
                }
                Command::SendWebSocket(frame) => {
                    self.websocket_mut()?
                        .send(Message::Text(frame.into()))
                        .await
                        .context("failed to send an o-sfu signaling frame")?;
                }
                Command::CreatePeerConnection => {
                    self.rtc = Some(Box::pin(RtcPeer::bind()).await?);
                }
                Command::ClosePeerConnection => {
                    self.rtc = None;
                }
                Command::ApplyNegotiation {
                    request_id,
                    kind,
                    sdp,
                    upload_slots,
                } => {
                    let answer = self.rtc_mut()?.answer_offer(&sdp, &upload_slots).await?;
                    pending.extend(
                        self.core
                            .submit_negotiation_answer(&request_id, kind, answer)
                            .into_vec(),
                    );
                    self.negotiation_count += 1;
                }
                Command::ScheduleTimer { id, ms } => {
                    let _ = self.timers.insert(id, ms);
                }
                Command::CancelTimer { id } => {
                    let _ = self.timers.remove(&id);
                }
                Command::CloseWebSocket { .. } => {
                    self.websocket_mut()?
                        .close(None)
                        .await
                        .context("failed to close the o-sfu WebSocket")?;
                }
                Command::EmitStateChange { .. }
                | Command::EmitEvent { .. }
                | Command::BeginPendingRequest { .. }
                | Command::ResolvePendingRequest { .. } => {}
            }
        }
        Ok(())
    }

    fn websocket_mut(&mut self) -> Result<&mut TestWebSocket> {
        self.websocket
            .as_mut()
            .context("protocol WebSocket is not connected")
    }

    fn rtc_mut(&mut self) -> Result<&mut RtcPeer> {
        self.rtc
            .as_mut()
            .context("protocol RTC peer is not available")
    }
}

impl LoadPeer {
    pub fn reset_send_pacing(&mut self) {
        self.rtc.reset_send_pacing();
    }

    pub async fn send_audio_packet(&mut self, packet: AudioPacket) -> Result<usize> {
        self.check_signaling().await?;
        let payload = self.rtc.send_audio_packet(packet).await?;
        self.check_signaling().await?;
        Ok(payload)
    }

    pub async fn read_rtp_payload(&mut self, timeout_window: Duration) -> Result<Option<Vec<u8>>> {
        self.check_signaling().await?;
        let payload = self.rtc.read_rtp_payload(timeout_window).await?;
        self.check_signaling().await?;
        Ok(payload)
    }

    pub async fn send_video_packet(&mut self, packet: VideoPacket) -> Result<usize> {
        self.check_signaling().await?;
        let payload = self.rtc.send_video_packet(packet).await?;
        self.check_signaling().await?;
        Ok(payload)
    }

    pub async fn close(mut self) -> Result<()> {
        if let Some(shutdown) = self.signaling_shutdown.take() {
            let _result = shutdown.send(());
        }
        self.wait_for_signaling().await
    }

    async fn check_signaling(&mut self) -> Result<()> {
        let is_finished = self
            .signaling_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished);
        if !is_finished {
            return Ok(());
        }
        self.wait_for_signaling().await?;
        Err(anyhow!("signaling task stopped during fixed work"))
    }

    async fn wait_for_signaling(&mut self) -> Result<()> {
        let task = self
            .signaling_task
            .take()
            .context("signaling task is not available")?;
        task.await.context("signaling task failed")?
    }
}

async fn read_text_message(websocket: &mut TestWebSocket) -> Result<String> {
    loop {
        match websocket
            .next()
            .await
            .context("o-sfu closed its signaling WebSocket")?
            .context("failed to read an o-sfu signaling frame")?
        {
            Message::Text(payload) => return Ok(payload.to_string()),
            Message::Ping(payload) => websocket
                .send(Message::Pong(payload))
                .await
                .context("failed to answer an o-sfu WebSocket ping")?,
            Message::Pong(_) => {}
            Message::Binary(_) | Message::Close(_) | Message::Frame(_) => {
                return Err(anyhow!("o-sfu sent an unexpected WebSocket frame"));
            }
        }
    }
}

async fn service_signaling(
    mut websocket: TestWebSocket,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    loop {
        tokio::select! {
            _result = &mut shutdown => {
                websocket
                    .close(None)
                    .await
                    .context("failed to close the fixed-work WebSocket")?;
                return Ok(());
            }
            message = websocket.next() => {
                match message
                    .context("o-sfu closed its fixed-work WebSocket")?
                    .context("failed to read the fixed-work WebSocket")?
                {
                    Message::Ping(payload) => websocket
                        .send(Message::Pong(payload))
                        .await
                        .context("failed to answer an o-sfu WebSocket ping")?,
                    Message::Pong(_) | Message::Text(_) => {}
                    Message::Binary(_) | Message::Close(_) | Message::Frame(_) => {
                        return Err(anyhow!(
                            "o-sfu sent an unexpected fixed-work WebSocket frame"
                        ));
                    }
                }
            }
        }
    }
}
