use std::fmt::Display;
use std::str;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use deadpool_redis::{cmd, Pool};
#[cfg(feature = "compression")]
use flate2::{Decompress, FlushDecompress, Status};
use futures::StreamExt;
use futures_util::SinkExt;
use log::{debug, error, info};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use url::Url;

use cache::{Cache, PostgresCache};
use common::event_forwarding;
#[cfg(feature = "whitelabel")]
use database::Database;
use model::guild::{Guild, Member};
use model::user::StatusUpdate;
use model::Snowflake;

use crate::config::Config;
use crate::gateway::payloads::PresenceUpdate;
use crate::gateway::whitelabel_utils::is_whitelabel;
use crate::gateway::GatewayError;

use super::payloads;
use super::payloads::event::Event;
use super::payloads::{Dispatch, Opcode, Payload};
use super::OutboundMessage;
use crate::gateway::event_forwarding::{is_whitelisted, EventForwarder};
use serde_json::error::Category;
use serde_json::Value;
use std::error::Error;
use tokio_tungstenite::{
    connect_async, tungstenite,
    tungstenite::{protocol::frame::coding::CloseCode, Message},
};

const GATEWAY_VERSION: u8 = 9;
const SEQ_SAVE_DELAY: Duration = Duration::from_secs(5);

pub struct Shard<T: EventForwarder> {
    pub(crate) config: Arc<Config>,
    pub(crate) identify: payloads::Identify,
    large_sharding_buckets: u16,
    cache: Arc<PostgresCache>,
    redis: Arc<Pool>,
    pub status_update_tx: mpsc::Sender<StatusUpdate>,
    status_update_rx: Mutex<mpsc::Receiver<StatusUpdate>>,
    pub(crate) user_id: Snowflake,
    total_rx: Mutex<u64>,
    seq: RwLock<Option<usize>>,
    last_seq_update: Mutex<Instant>,
    session_id: RwLock<Option<String>>,
    writer: RwLock<Option<mpsc::Sender<OutboundMessage>>>,
    kill_heartbeat: Mutex<Option<oneshot::Sender<()>>>,
    pub kill_shard_tx: Mutex<Option<oneshot::Sender<()>>>,
    kill_shard_rx: Mutex<oneshot::Receiver<()>>,
    last_ack: RwLock<Instant>,
    last_heartbeat: RwLock<Instant>,
    connect_time: RwLock<Instant>,
    ready_tx: Mutex<Option<oneshot::Sender<()>>>,
    ready_guild_count: AtomicU16,
    received_count: AtomicU16,
    is_ready: AtomicBool,
    pub(crate) event_forwarder: Arc<T>,

    #[cfg(feature = "whitelabel")]
    pub(crate) database: Arc<Database>,
}

#[cfg(feature = "compression")]
const CHUNK_SIZE: usize = 16 * 1024; // 16KiB

impl<T: EventForwarder> Shard<T> {
    pub fn new(
        config: Arc<Config>,
        identify: payloads::Identify,
        large_sharding_buckets: u16,
        cache: Arc<PostgresCache>,
        redis: Arc<Pool>,
        user_id: Snowflake,
        event_forwarder: Arc<T>,
        #[cfg(feature = "whitelabel")] database: Arc<Database>,
    ) -> Arc<Shard<T>> {
        let (kill_shard_tx, kill_shard_rx) = oneshot::channel();
        let (status_update_tx, status_update_rx) = mpsc::channel(1);

        Arc::new(Shard {
            config,
            identify,
            large_sharding_buckets,
            cache,
            redis,
            status_update_tx,
            status_update_rx: Mutex::new(status_update_rx),
            user_id,
            total_rx: Mutex::new(0),
            seq: RwLock::new(None),
            last_seq_update: Mutex::new(Instant::now()),
            session_id: RwLock::new(None),
            writer: RwLock::new(None),
            kill_heartbeat: Mutex::new(None),
            kill_shard_tx: Mutex::new(Some(kill_shard_tx)),
            kill_shard_rx: Mutex::new(kill_shard_rx),
            last_ack: RwLock::new(Instant::now()),
            last_heartbeat: RwLock::new(Instant::now()),
            connect_time: RwLock::new(Instant::now()), // will be overwritten
            ready_tx: Mutex::new(None),
            ready_guild_count: AtomicU16::new(0),
            received_count: AtomicU16::new(0),
            is_ready: AtomicBool::new(false),
            event_forwarder,
            #[cfg(feature = "whitelabel")]
            database,
        })
    }

    pub async fn connect(
        self: Arc<Self>,
        ready_tx: Option<oneshot::Sender<()>>,
    ) -> Result<(), GatewayError> {
        //rst
        *self.ready_tx.lock().await = ready_tx;

        let (kill_shard_tx, kill_shard_rx) = oneshot::channel();
        *self.kill_shard_tx.lock().await = Some(kill_shard_tx);
        *self.kill_shard_rx.lock().await = kill_shard_rx;

        *self.total_rx.lock().await = 0;
        self.ready_guild_count.store(0, Ordering::Relaxed);
        self.received_count.store(0, Ordering::Relaxed);
        self.is_ready.store(false, Ordering::Relaxed);

        *self.last_heartbeat.write().await = Instant::now();
        *self.last_ack.write().await = Instant::now();
        // rst

        let mut uri = format!(
            "wss://gateway.discord.gg/?v={}&encoding=json",
            GATEWAY_VERSION
        );
        if cfg!(feature = "compression") {
            uri.push_str("&compress=zlib-stream");
        }

        let uri = Url::parse(&uri[..]).expect("Failed to parse websocket uri");

        let (wss, _) = connect_async(uri).await?;
        let (ws_tx, ws_rx) = wss.split();
        *self.connect_time.write().await = Instant::now();

        // start writer
        let (recv_broker_tx, recv_broker_rx) = futures::channel::mpsc::unbounded();
        let (send_broker_tx, send_broker_rx) = futures::channel::mpsc::unbounded();
        let (internal_tx, internal_rx) = mpsc::channel(1);
        tokio::spawn(handle_writes(send_broker_tx, internal_rx));

        let forward_outbound = send_broker_rx.map(Ok).forward(ws_tx);
        let forward_inbound = ws_rx.map(Ok).forward(recv_broker_tx);

        *self.writer.write().await = Some(internal_tx);

        tokio::spawn(async move {
            futures::future::select(forward_outbound, forward_inbound).await;
        });

        // start read loop
        if let Err(e) = self.listen(recv_broker_rx).await {
            return Err(e);
        }

        Ok(())
    }

    // helper function
    async fn write<U: Serialize>(
        &self,
        msg: U,
        tx: oneshot::Sender<Result<(), futures::channel::mpsc::SendError>>,
    ) -> Result<(), GatewayError> {
        OutboundMessage::new(msg, tx)?
            .send(self.writer.read().await.clone().unwrap())
            .await?;

        Ok(())
    }

    // helper function
    pub fn kill(self: Arc<Self>) {
        // BIG problem
        // TODO: Make this good
        tokio::spawn(async move {
            // TODO: panic?
            let kill_shard_tx = self.kill_shard_tx.lock().await.take();
            let kill_heartbeat_tx = self.kill_heartbeat.lock().await.take();

            match kill_shard_tx {
                Some(kill_shard_tx) => {
                    if kill_shard_tx.send(()).is_err() {
                        self.log_err(
                            "Failed to kill",
                            &GatewayError::custom("Receiver already unallocated"),
                        );
                    }
                }
                None => self.log("Tried to kill but kill_shard_tx was None"),
            }

            match kill_heartbeat_tx {
                Some(kill_heartbeat_tx) => {
                    if kill_heartbeat_tx.send(()).is_err() {
                        self.log_err(
                            "Failed to kill heartbeat",
                            &GatewayError::custom("Receiver already unallocated"),
                        );
                    }
                }
                None => self.log("Tried to kill but kill_heartbeat_tx was None"),
            }
        });
    }

    async fn listen(
        self: Arc<Self>,
        mut rx: futures::channel::mpsc::UnboundedReceiver<
            Result<Message, tokio_tungstenite::tungstenite::Error>,
        >,
    ) -> Result<(), GatewayError> {
        #[cfg(feature = "compression")]
        let mut decoder = Decompress::new(true);

        loop {
            let shard = Arc::clone(&self);
            let kill_rx = &mut *shard.kill_shard_rx.lock().await;
            let status_update_rx = &mut shard.status_update_rx.lock().await;

            tokio::select! {
                // handle kill
                _ = kill_rx => {
                    self.log("Received kill message");
                    break;
                }

                // handle incoming payload
                payload = rx.next() => {
                    match payload {
                        None => {
                            self.log("Payload was None, killing");
                            self.kill();
                            break;
                        }

                        Some(Err(e)) => {
                            self.log_err("Error reading data from websocket, killing", &GatewayError::WebsocketError(e));
                            self.kill();
                            break;
                        }

                        Some(Ok(Message::Close(frame))) => {
                            self.log(format!("Got close from gateway: {:?}", frame));
                            Arc::clone(&self).kill();

                            if let Some(frame) = frame {
                                if let CloseCode::Library(code) = frame.code {
                                    let fatal_codes: [u16; 2] = [4004, 4014];
                                    if fatal_codes.contains(&code) {
                                        return GatewayError::AuthenticationError {
                                            bot_token: self.identify.data.token.clone(),
                                            error_code: frame.code,
                                            error: frame.reason.to_string(),
                                        }.into();
                                    }
                                }
                            }

                            break;
                        }

                        Some(Ok(Message::Text(data))) => {
                            let value: Value = serde_json::from_slice(data.as_bytes())?;

                            let payload = match Arc::clone(&self).read_payload(&value).await {
                                Ok(payload) => payload,
                                Err(e) => {
                                    self.log_err("Error while deserializing payload", &e);
                                    continue;
                                }
                            };

                            if let Err(e) = Arc::clone(&self).process_payload(payload, value).await {
                                self.log_err("An error occurred while processing a payload", &e);
                            }
                        }

                        #[cfg(feature = "compression")]
                        Some(Ok(Message::Binary(data))) => {
                            let data = match Arc::clone(&self).decompress(data, &mut decoder).await {
                                Ok(data) => data,
                                Err(e) => {
                                    self.log_err("Error while decompressing payload", &e);
                                    continue;
                                }
                            };

                            let value: Value = serde_json::from_slice(data.as_bytes())?;

                            let payload = match Arc::clone(&self).read_payload(&value).await {
                                Ok(payload) => payload,
                                Err(e) => {
                                    self.log_err("Error while deserializing payload", &e);
                                    continue;
                                }
                            };

                            if let Err(e) = Arc::clone(&self).process_payload(payload, value).await {
                                self.log_err("An error occurred while processing a payload", &e);
                            }
                        }

                        _ => {}
                    }
                }

                // handle status update
                presence = status_update_rx.recv() => {
                    if let Some(presence) = presence {
                        let (tx, rx) = oneshot::channel();

                        let shard = Arc::clone(&self);
                        tokio::spawn(async move {
                            let payload = PresenceUpdate::new(presence);
                            if let Err(e) = shard.write(payload, tx).await {
                                shard.log_err("Error sending presence update payload to writer", &e);
                            }

                            match rx.await {
                                Ok(Err(e)) => shard.log_err("Error writing presence update payload", &GatewayError::WebsocketSendError(e)),
                                Err(e) => shard.log_err("Error writing presence update payload", &GatewayError::RecvError(e)),
                                _ => {}
                            }
                        });
                    }
                }
            }
        }

        Ok(())
    }

    #[cfg(feature = "compression")]
    async fn decompress(
        self: Arc<Self>,
        data: Vec<u8>,
        decoder: &mut Decompress,
    ) -> Result<Vec<u8>, GatewayError> {
        let mut total_rx = self.total_rx.lock().await;

        let mut output: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
        let before = total_rx.clone();
        let mut offset: usize = 0;

        while ((decoder.total_in() - *total_rx) as usize) < data.len() {
            let mut temp: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);

            match decoder
                .decompress_vec(&data[offset..], &mut temp, FlushDecompress::Sync)
                .map_err(GatewayError::DecompressError)
            {
                Ok(Status::StreamEnd) => break,
                Ok(Status::Ok) | Ok(Status::BufError) => {
                    output.append(&mut temp);
                    offset = (decoder.total_in() - before) as usize;
                }

                // TODO: Should we reconnect?
                Err(e) => {
                    *total_rx = 0;
                    decoder.reset(true);

                    return Err(e);
                }
            };
        }

        *total_rx = decoder.total_in();

        return Ok(output);
    }

    // Manually deserialize since we only need 2 values
    async fn read_payload(self: Arc<Self>, data: &Value) -> Result<Payload, GatewayError> {
        let opcode = serde_json::from_value(
            data.get("op")
                .ok_or_else(|| GatewayError::MissingFieldError("op".to_owned()))?
                .clone(),
        )?;

        let seq = match data.get("s") {
            None => None,
            Some(s) => serde_json::from_value(s.clone())?,
        };

        Ok(Payload { opcode, seq })
    }

    async fn process_payload(
        self: Arc<Self>,
        payload: Payload,
        raw: Value,
    ) -> Result<(), GatewayError> {
        if let Some(seq) = payload.seq {
            *self.seq.write().await = Some(seq);

            let mut last_updated = self.last_seq_update.lock().await;
            if last_updated.elapsed() > SEQ_SAVE_DELAY {
                *last_updated = Instant::now();
                drop(last_updated);

                if let Err(e) = self.save_seq().await {
                    self.log_err("Error saving sequence number", &e);
                }
            }
        }

        match payload.opcode {
            Opcode::Dispatch => {
                let payload = serde_json::from_value(raw)?;

                if let Err(e) = Arc::clone(&self).handle_event(payload).await {
                    if let GatewayError::JsonError(ref err) = e {
                        // Ignore unknown payloads
                        if err.classify() != Category::Data {
                            self.log_err("Error processing dispatch", &e);
                        }
                    } else {
                        self.log_err("Error processing dispatch", &e);
                    }
                }
            }

            Opcode::Reconnect => {
                self.log("Received reconnect payload from Discord");
                self.kill();
            }

            Opcode::InvalidSession => {
                self.log("Received invalid session payload from Discord");

                *self.session_id.write().await = None;
                *self.seq.write().await = None;

                // delete session ID from Redis
                if let Err(e) = self.delete_session_id().await {
                    self.log_err("Error deleting session_id from Redis", &e);
                }

                // delete seq from Redis
                if let Err(e) = self.delete_seq().await {
                    self.log_err("Error deleting seq from Redis", &e);
                }

                self.kill();
            }

            Opcode::Hello => {
                let hello: payloads::Hello = serde_json::from_value(raw)?;
                let interval = Duration::from_millis(hello.data.heartbeat_interval as u64);

                let mut should_identify = true;

                // try to load session_id from redis
                if let Ok(Some(session_id)) = self.load_session_id().await {
                    *self.session_id.write().await = Some(session_id);

                    // if success, load seq from redis
                    if let Ok(Some(seq)) = self.load_seq().await {
                        *self.seq.write().await = Some(seq)
                    }
                }

                let session_id = self.session_id.read().await.as_ref().cloned();
                let seq = *self.seq.read().await;
                if let (Some(session_id), Some(seq)) = (session_id, seq) {
                    if let Err(e) = Arc::clone(&self).do_resume(session_id.clone(), seq).await {
                        self.log_err("Error RESUMEing, going to IDENTIFY", &e);

                        // rst
                        *self.session_id.write().await = None;
                        *self.seq.write().await = None;

                        self.wait_for_ratelimit().await?;

                        if self.connect_time.read().await.elapsed() > interval {
                            self.log(
                                "Connected over 45s ago, Discord will kick us off. Reconnecting.",
                            );
                            Arc::clone(&self).kill();
                            return Ok(());
                        } else {
                            should_identify = Arc::clone(&self).do_identify().await.is_err();
                        }
                    } else {
                        should_identify = false;
                        self.log("Sent resume successfully");
                    }
                }

                if should_identify {
                    self.wait_for_ratelimit().await?;

                    if self.connect_time.read().await.elapsed() > interval {
                        self.log("Connected over 45s ago, Discord will kick us off. Reconnecting.");
                        self.kill();
                        return Ok(());
                    }

                    if let Err(e) = Arc::clone(&self).do_identify().await {
                        self.log_err("Error identifying, killing", &e);
                        self.kill();
                        return e.into();
                    }

                    self.log("Identified");
                }

                let kill_tx = Arc::clone(&self).start_heartbeat(interval).await;
                *self.kill_heartbeat.lock().await = Some(kill_tx)
            }

            Opcode::HeartbeatAck => {
                *self.last_ack.write().await = Instant::now();

                // save session ID
                if let Err(e) = self.save_session_id().await {
                    self.log_err("Error occurred while saving session ID", &e);
                }
            }

            _ => {}
        }

        Ok(())
    }

    async fn handle_event(self: Arc<Self>, data: Box<RawValue>) -> Result<(), GatewayError> {
        let payload: Dispatch = serde_json::from_str(data.get())?;

        // Gateway events
        match &payload.data {
            Event::Ready(ready) => {
                *self.session_id.write().await = Some(ready.session_id.clone());
                if let Err(e) = self.save_session_id().await {
                    self.log_err("Error saving session ID to Redis", &e);
                }

                self.ready_guild_count
                    .store(ready.guilds.len() as u16, Ordering::Relaxed);

                self.log(format!(
                    "Ready on {}#{} ({})",
                    ready.user.username, ready.user.discriminator, ready.user.id
                ));
                return Ok(());
            }

            Event::Resumed(_) => {
                self.log("Received resumed acknowledgement");

                if !self
                    .is_ready
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .unwrap_or_else(|x| x)
                {
                    if let Some(tx) = self.ready_tx.lock().await.take() {
                        if tx.send(()).is_err() {
                            self.log_err(
                                "Error sending ready notification to probe",
                                &GatewayError::ReceiverHungUpError,
                            );
                        }
                    }
                }

                return Ok(());
            }

            #[cfg(not(feature = "whitelabel"))]
            Event::GuildCreate(_) => {
                self.update_count().await;
            }

            #[cfg(feature = "whitelabel")]
            Event::GuildCreate(g) => {
                self.update_count().await;

                #[cfg(feature = "whitelabel")]
                if let Err(e) = self.store_whitelabel_guild(g.id).await {
                    self.log_err("Error while storing whitelabel guild data", &e);
                }
            }

            _ => {}
        }

        // cache + push to redis
        tokio::spawn(async move {
            let guild_id = super::event_forwarding::get_guild_id(&payload.data);
            let should_forward =
                is_whitelisted(&payload.data) && self.meets_forward_threshold(&payload.data).await;

            // cache
            let res = match payload.data {
                Event::ChannelCreate(channel) => self.cache.store_channel(channel).await,
                Event::ChannelUpdate(channel) => self.cache.store_channel(channel).await,
                Event::ChannelDelete(channel) => self.cache.delete_channel(channel.id).await,
                Event::ThreadCreate(thread) => self.cache.store_channel(thread).await,
                Event::ThreadUpdate(thread) => self.cache.store_channel(thread).await,
                Event::ThreadDelete(thread) => self.cache.delete_channel(thread.id).await,
                Event::GuildCreate(mut guild) => {
                    apply_guild_id_to_channels(&mut guild);
                    self.cache.store_guild(guild).await
                }
                Event::GuildUpdate(mut guild) => {
                    apply_guild_id_to_channels(&mut guild);
                    self.cache.store_guild(guild).await
                }
                Event::GuildDelete(guild) => {
                    if guild.unavailable.is_none() {
                        // we were kicked
                        // TODO: don't delete if this is main bot & whitelabel bot is in guild
                        self.cache.delete_guild(guild.id).await
                    } else {
                        Ok(())
                    }
                }
                Event::GuildBanAdd(ev) => self.cache.delete_member(ev.user.id, ev.guild_id).await,
                Event::GuildEmojisUpdate(ev) => {
                    self.cache.store_emojis(ev.emojis, ev.guild_id).await
                }
                Event::GuildMemberAdd(ev) => self.cache.store_member(ev.member, ev.guild_id).await,
                Event::GuildMemberRemove(ev) => {
                    self.cache.delete_member(ev.user.id, ev.guild_id).await
                }
                Event::GuildMemberUpdate(ev) => {
                    self.cache
                        .store_member(
                            Member {
                                user: Some(ev.user),
                                nick: ev.nick,
                                roles: ev.roles,
                                joined_at: ev.joined_at,
                                premium_since: ev.premium_since,
                                deaf: false, // TODO: Don't update these fields somehow?
                                mute: false, // TODO: Don't update these fields somehow?
                            },
                            ev.guild_id,
                        )
                        .await
                }
                Event::GuildMembersChunk(ev) => {
                    self.cache.store_members(ev.members, ev.guild_id).await
                }
                Event::GuildRoleCreate(ev) => self.cache.store_role(ev.role, ev.guild_id).await,
                Event::GuildRoleUpdate(ev) => self.cache.store_role(ev.role, ev.guild_id).await,
                Event::GuildRoleDelete(ev) => self.cache.delete_role(ev.role_id).await,
                Event::UserUpdate(user) => self.cache.store_user(user).await,
                _ => Ok(()),
            };

            if let Err(e) = res {
                self.log_err("Error updating cache", &GatewayError::CacheError(e));
            }

            // push to workers, even if error occurred
            if should_forward {
                // prepare payload
                let wrapped = event_forwarding::Event {
                    bot_token: &self.identify.data.token[..],
                    bot_id: self.user_id.0,
                    is_whitelabel: is_whitelabel(),
                    shard_id: self.get_shard_id(),
                    event: &data,
                };

                if let Err(e) = self
                    .event_forwarder
                    .forward_event(&*self.config, wrapped, guild_id)
                    .await
                {
                    self.log_err("Error while executing worker HTTP request", &e);
                }
            }
        });

        Ok(())
    }

    async fn update_count(&self) {
        if !self.is_ready.load(Ordering::Relaxed) {
            let received = self.received_count.fetch_add(1, Ordering::Relaxed);

            if received >= (self.ready_guild_count.load(Ordering::Relaxed) / 100) * 90 {
                // Once we have 90% of the guilds, we're ok to load more shards
                // CAS in case value was updated since read
                if !self
                    .is_ready
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .unwrap_or_else(|x| x)
                {
                    if let Some(tx) = self.ready_tx.lock().await.take() {
                        self.log("Reporting readiness");
                        if tx.send(()).is_err() {
                            self.log_err(
                                "Error sending ready notification to probe",
                                &GatewayError::ReceiverHungUpError,
                            );
                        }
                        self.log("Reported readiness");
                    }
                }
            }
        }
    }

    async fn meets_forward_threshold(&self, event: &Event) -> bool {
        if cfg!(feature = "skip-initial-guild-creates") {
            if let Event::GuildCreate(_) = event {
                // if not ready, don't forward event
                return self.is_ready.load(Ordering::Relaxed);
            }
        }

        true
    }

    // returns cancellation channel
    async fn start_heartbeat(self: Arc<Self>, interval: Duration) -> oneshot::Sender<()> {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();

        tokio::spawn(async move {
            sleep(interval).await;

            let mut has_done_heartbeat = false;
            while let Err(oneshot::error::TryRecvError::Empty) = cancel_rx.try_recv() {
                let shard = Arc::clone(&self);

                // if done inline, clippy complains about evaluation order
                let last_ack = shard.last_ack.read().await;
                let last_heartbeat = shard.last_heartbeat.read().await;
                let elapsed = last_ack.checked_duration_since(*last_heartbeat);
                drop(last_heartbeat); // drop here so that a reference is no longer held to shard
                drop(last_ack);

                if has_done_heartbeat && (elapsed.is_none() || elapsed.unwrap() > interval) {
                    shard.log("Hasn't received heartbeat ack, killing");
                    shard.kill();
                    break;
                }

                if let Err(e) = Arc::clone(&shard).do_heartbeat().await {
                    shard.log_err("Error sending heartbeat, killing", &e);
                    shard.kill();
                    break;
                }

                has_done_heartbeat = true;

                sleep(interval).await;
            }
        });

        cancel_tx
    }

    async fn do_heartbeat(self: Arc<Self>) -> Result<(), GatewayError> {
        let payload = payloads::Heartbeat::new(*self.seq.read().await);

        let (tx, rx) = oneshot::channel();
        self.write(payload, tx).await?;

        rx.await??;

        *self.last_heartbeat.write().await = Instant::now();
        Ok(())
    }

    async fn do_identify(self: Arc<Self>) -> Result<(), GatewayError> {
        let (tx, rx) = oneshot::channel();
        self.write(&self.identify, tx).await?;

        Ok(rx.await??)
    }

    async fn wait_for_ratelimit(&self) -> Result<(), GatewayError> {
        let key = if is_whitelabel() {
            format!("ratelimiter:whitelabel:identify:{}", self.user_id)
        } else {
            format!(
                "ratelimiter:public:identify:{}",
                self.get_shard_id() % self.large_sharding_buckets
            )
        };

        let mut res = redis::Value::Nil;
        while res == redis::Value::Nil {
            let mut conn = self.redis.get().await?;

            res = cmd("SET")
                .arg(&[&key[..], "1", "NX", "PX", "6000"]) // some arbitrary value, set if not exist, set expiry, of 6s
                .query_async(&mut conn)
                .await?;

            if res == redis::Value::Nil {
                // get time to delay
                let ttl = cmd("PTTL").arg(&key).query_async(&mut conn).await?;

                if let redis::Value::Int(ttl) = ttl {
                    // if number is negative, we can go ahead and identify
                    // -1 = no expire, -2 = doesn't exist
                    if ttl > 0 {
                        let ttl = Duration::from_millis(ttl as u64);
                        sleep(ttl).await
                    }
                }
            }
        }

        Ok(())
    }

    /// Shard.session_id & Shard.seq should not be None when calling this function
    /// if they are, the function will panic
    async fn do_resume(
        self: Arc<Self>,
        session_id: String,
        seq: usize,
    ) -> Result<(), GatewayError> {
        let payload = payloads::Resume::new(self.identify.data.token.clone(), session_id, seq);

        let (tx, rx) = oneshot::channel();
        self.write(payload, tx).await?;

        Ok(rx.await??)
    }

    async fn save_session_id(&self) -> Result<(), GatewayError> {
        match &*self.session_id.read().await {
            Some(session_id) => {
                let mut conn = self.redis.get().await?;

                let key = match self.get_resume_key().await {
                    Some(key) => key,
                    None => return Ok(()),
                };

                cmd("SET")
                    .arg(&[&key[..], session_id, "EX", "120"]) // expiry of 120s
                    .query_async(&mut conn)
                    .await?;

                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn load_session_id(&self) -> Result<Option<String>, GatewayError> {
        let key = match self.get_resume_key().await {
            Some(key) => key,
            None => return Ok(None),
        };

        let mut conn = self.redis.get().await?;

        let res = cmd("GET").arg(&[&key[..]]).query_async(&mut conn).await?;

        match res {
            redis::Value::Data(data) => {
                let session_id = str::from_utf8(&data[..])?.to_owned();
                Ok(Some(session_id))
            }
            _ => Ok(None),
        }
    }

    async fn save_seq(&self) -> Result<(), GatewayError> {
        match &*self.seq.read().await {
            Some(seq) => {
                let mut conn = self.redis.get().await?;

                let key = match self.get_seq_key().await {
                    Some(key) => key,
                    None => return Ok(()),
                };

                cmd("SET")
                    .arg(&[&key[..], &seq.to_string()[..], "EX", "120"]) // expiry of 120s
                    .query_async(&mut conn)
                    .await?;

                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn load_seq(&self) -> Result<Option<usize>, GatewayError> {
        let key = match self.get_seq_key().await {
            Some(key) => key,
            None => return Ok(None),
        };

        let mut conn = self.redis.get().await?;

        let res = cmd("GET").arg(&[&key[..]]).query_async(&mut conn).await?;

        let seq = match res {
            redis::Value::Data(data) => str::from_utf8(&data[..])?.parse().ok(),
            _ => None,
        };

        Ok(seq)
    }

    async fn delete_session_id(&self) -> Result<(), GatewayError> {
        let mut conn = self.redis.get().await?;

        let key = match self.get_resume_key().await {
            Some(key) => key,
            None => return Ok(()),
        };

        cmd("DEL").arg(&[&key[..]]).query_async(&mut conn).await?;

        Ok(())
    }

    async fn delete_seq(&self) -> Result<(), GatewayError> {
        let mut conn = self.redis.get().await?;

        let key = match self.get_seq_key().await {
            Some(key) => key,
            None => return Ok(()),
        };

        cmd("DEL").arg(&[&key[..]]).query_async(&mut conn).await?;

        Ok(())
    }

    async fn get_resume_key(&self) -> Option<String> {
        if is_whitelabel() {
            Some(format!(
                "tickets:resume:{}:{}",
                self.user_id,
                self.get_shard_id()
            ))
        } else {
            Some(format!(
                "tickets:resume:public:{}-{}",
                self.get_shard_id(),
                self.identify.data.shard_info.num_shards
            ))
        }
    }

    async fn get_seq_key(&self) -> Option<String> {
        if is_whitelabel() {
            Some(format!(
                "tickets:seq:{}:{}",
                self.user_id,
                self.get_shard_id()
            ))
        } else {
            Some(format!(
                "tickets:seq:public:{}-{}",
                self.get_shard_id(),
                self.identify.data.shard_info.num_shards
            ))
        }
    }

    /// helper
    pub fn get_shard_id(&self) -> u16 {
        self.identify.data.shard_info.shard_id
    }

    pub fn log(&self, msg: impl Display) {
        if is_whitelabel() {
            info!("[shard:{}] {}", self.user_id, msg);
        } else {
            info!("[shard:{:0>2}] {}", self.get_shard_id(), msg);
        }
    }

    pub fn log_err(&self, msg: impl Display, err: &GatewayError) {
        if is_whitelabel() {
            error!("[shard:{}] {}: {}", self.user_id, msg, err);
        } else {
            error!("[shard:{:0>2}] {}: {}", self.get_shard_id(), msg, err);
        }
    }

    pub fn log_debug(&self, msg: impl Display, raw_payload: &str, err: impl Error) {
        if is_whitelabel() {
            debug!(
                "[shard:{}] {}: {}\nFull payload: {}",
                self.user_id, msg, err, raw_payload
            );
        } else {
            debug!(
                "[shard:{:0>2}] {}: {}\nFull payload: {}",
                self.get_shard_id(),
                msg,
                err,
                raw_payload
            );
        }
    }
}

async fn handle_writes(
    mut tx: futures::channel::mpsc::UnboundedSender<tungstenite::Message>,
    mut rx: mpsc::Receiver<super::OutboundMessage>,
) {
    while let Some(msg) = rx.recv().await {
        let payload = Message::text(msg.message);
        let res = tx.send(payload).await;

        if let Err(e) = msg.tx.send(res) {
            eprintln!("Error while sending write result back to caller: {:?}", e);
        }
    }
}

fn apply_guild_id_to_channels(guild: &mut Guild) {
    if let Some(channels) = &mut guild.channels {
        for channel in channels {
            channel.guild_id = Some(guild.id)
        }
    }

    if let Some(threads) = &mut guild.threads {
        for thread in threads {
            thread.guild_id = Some(guild.id)
        }
    }
}
