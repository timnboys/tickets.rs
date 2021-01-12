use async_trait::async_trait;
use crate::{Shard, GatewayError};
use common::event_forwarding;
use std::sync::Arc;
use std::time::Duration;
use crate::gateway::worker_response::WorkerResponse;
use crate::gateway::payloads::event::Event;
use model::Snowflake;
use tokio::time::delay_for;
use tokio::sync::RwLock;
use crate::event_forwarding::EventForwarder;

pub struct HttpEventForwarder {
    client: reqwest::Client,
    cookie: RwLock<Option<Box<str>>>,
}

impl HttpEventForwarder {
    pub fn new(client: reqwest::Client) -> HttpEventForwarder {
        HttpEventForwarder {
            client,
            cookie: RwLock::new(Option::None),
        }
    }

    pub fn build_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .gzip(cfg!(feature = "compression"))
            .build()
            .expect("build_http_client")
    }

    pub fn start_reset_cookie_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                delay_for(Duration::from_secs(180)).await;
                *self.cookie.write().await = None;
            }
        });
    }
}

#[async_trait]
impl EventForwarder for HttpEventForwarder {
    async fn forward_event(&self, shard: Arc<Shard>, event: event_forwarding::Event<'_>, guild_id: Option<Snowflake>) -> Result<(), GatewayError> {
        let uri = &*shard.config.worker_svc_uri;

        // reqwest::Client uses Arcs internally, meaning this method clones the same client but
        // allows us to make use of connection pooling
        let mut req = self.client.clone()
            .post(uri)
            .json(&event);

        if let Some(guild_id) = guild_id {
            let header_name = &*shard.config.sticky_cookie;
            req = req.header(header_name, guild_id.0);
        }

        let cookie = self.cookie.read().await;
        if let Some(cookie) = &*cookie {
            let value = format!("{}={}", shard.config.sticky_cookie, cookie);
            req = req.header(reqwest::header::COOKIE, value);
        }
        drop(cookie); // drop here so we can write later

        let res = req.send()
            .await
            .map_err(GatewayError::ReqwestError)?;

        if let Some(cookie) = res.cookies().find(|c| c.name() == &*shard.config.sticky_cookie) {
            shard.log(format!("Got new session cookie: {}", cookie.value()));
            *self.cookie.write().await = Some(Box::from(cookie.value()));
        }

        let res: WorkerResponse = res.json()
            .await
            .map_err(GatewayError::ReqwestError)?;

        match res.success {
            true => Ok(()),
            false => GatewayError::WorkerError(res.error.unwrap_or_else(|| "No error found".to_owned())).into()
        }
    }
}