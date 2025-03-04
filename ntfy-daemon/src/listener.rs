use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::task::{self, spawn_local, LocalSet};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_stream::wrappers::LinesStream;
use tracing::{debug, error, info, warn, Instrument, Span};

use crate::credentials::Credentials;
use crate::http_client::HttpClient;
use crate::{models, Error};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ServerEvent {
    #[serde(rename = "open")]
    Open {
        id: String,
        time: usize,
        expires: Option<usize>,
        topic: String,
    },
    #[serde(rename = "message")]
    Message(models::ReceivedMessage),
    #[serde(rename = "keepalive")]
    KeepAlive {
        id: String,
        time: usize,
        expires: Option<usize>,
        topic: String,
    },
}

#[derive(Debug, Clone)]
pub enum ListenerEvent {
    Message(models::ReceivedMessage),
    ConnectionStateChanged(ConnectionState),
}

#[derive(Clone)]
pub struct ListenerConfig {
    pub(crate) http_client: HttpClient,
    pub(crate) credentials: Credentials,
    pub(crate) endpoint: String,
    pub(crate) topic: String,
    pub(crate) since: u64,
}

#[derive(Debug)]
pub enum ListenerCommand {
    Restart,
    Shutdown,
    GetState(oneshot::Sender<ConnectionState>),
}

fn topic_request(
    client: &HttpClient,
    endpoint: &str,
    topic: &str,
    since: u64,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<reqwest::Request> {
    let url = models::Subscription::build_url(endpoint, topic, since)?;
    let mut req = client
        .get(url.as_str())
        .header("Content-Type", "application/x-ndjson")
        .header("Transfer-Encoding", "chunked");
    if let Some(username) = username {
        req = req.basic_auth(username, password);
    }

    Ok(req.build()?)
}

async fn response_lines(
    res: impl tokio::io::AsyncBufRead,
) -> Result<impl futures::Stream<Item = Result<String, std::io::Error>>, reqwest::Error> {
    let lines = LinesStream::new(res.lines());
    Ok(lines)
}

#[derive(Clone, Debug)]
pub enum ConnectionState {
    Unitialized,
    Connected,
    Reconnecting {
        retry_count: u64,
        delay: Duration,
        error: Option<Arc<anyhow::Error>>,
    },
}

pub struct ListenerActor {
    pub event_tx: async_channel::Sender<ListenerEvent>,
    pub commands_rx: Option<mpsc::Receiver<ListenerCommand>>,
    pub config: ListenerConfig,
    pub state: ConnectionState,
}

impl ListenerActor {
    pub async fn run_loop(mut self) {
        let span = tracing::info_span!("listener_loop", topic = %self.config.topic);
        async {
            let mut commands_rx = self.commands_rx.take().unwrap();
            loop {
                select! {
                    _ = self.run_supervised_loop() => {
                        info!("supervised loop ended");
                        break;
                    },
                    cmd = commands_rx.recv() => {
                        match cmd {
                            Some(ListenerCommand::Restart) => {
                                info!("restarting listener");
                                continue;
                            }
                            Some(ListenerCommand::Shutdown) => {
                                info!("shutting down listener");
                                break;
                            }
                            Some(ListenerCommand::GetState(tx)) => {
                                debug!("getting listener state");
                                let state = self.state.clone();
                                if tx.send(state).is_err() {
                                    warn!("failed to send state - receiver dropped");
                                }
                            }
                            None => {
                                error!("command channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        }
        .instrument(span)
        .await;
    }

    async fn set_state(&mut self, state: ConnectionState) {
        self.state = state.clone();
        self.event_tx
            .send(ListenerEvent::ConnectionStateChanged(state))
            .await
            .unwrap();
    }
    async fn run_supervised_loop(&mut self) {
        let span = tracing::info_span!("supervised_loop");
        async {
            let retrier = || {
                crate::retry::WaitExponentialRandom::builder()
                    .min(Duration::from_secs(1))
                    .max(Duration::from_secs(5 * 60))
                    .build()
            };
            let mut retry = retrier();
            loop {
                let start_time = std::time::Instant::now();

                if let Err(e) = self.recv_and_forward_loop().await {
                    let uptime = std::time::Instant::now().duration_since(start_time);
                    // Reset retry delay to minimum if uptime was decent enough
                    if uptime > Duration::from_secs(60 * 4) {
                        debug!("resetting retry delay due to sufficient uptime");
                        retry = retrier();
                    }
                    error!(error = ?e, "connection error");
                    self.set_state(ConnectionState::Reconnecting {
                        retry_count: retry.count(),
                        delay: retry.next_delay(),
                        error: Some(Arc::new(e)),
                    })
                    .await;
                    info!(delay = ?retry.next_delay(), "waiting before reconnect attempt");
                    retry.wait().await;
                } else {
                    break;
                }
            }
        }
        .instrument(span)
        .await;
    }

    async fn recv_and_forward_loop(&mut self) -> anyhow::Result<()> {
        let span = tracing::info_span!("receive_loop",
            endpoint = %self.config.endpoint,
            topic = %self.config.topic,
            since = %self.config.since
        );
        async {
            let creds = self.config.credentials.get(&self.config.endpoint);
            debug!("creating request");
            let req = topic_request(
                &self.config.http_client,
                &self.config.endpoint,
                &self.config.topic,
                self.config.since,
                creds.as_ref().map(|x| x.username.as_str()),
                creds.as_ref().map(|x| x.password.as_str()),
            );

            debug!("executing request");
            let res = self.config.http_client.execute(req?).await?;
            let res = res.error_for_status()?;
            let reader = tokio_util::io::StreamReader::new(
                res.bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
            );
            let stream = response_lines(reader).await?;
            tokio::pin!(stream);

            self.set_state(ConnectionState::Connected).await;
            info!("connection established");

            info!(topic = %&self.config.topic, "listening");
            while let Some(msg) = stream.next().await {
                let msg = msg?;

                let min_msg = serde_json::from_str::<models::MinMessage>(&msg)
                    .map_err(|e| Error::InvalidMinMessage(msg.to_string(), e))?;
                self.config.since = min_msg.time.max(self.config.since);

                let event = serde_json::from_str(&msg)
                    .map_err(|e| Error::InvalidMessage(msg.to_string(), e))?;

                match event {
                    ServerEvent::Message(msg) => {
                        debug!(id = %msg.id, "forwarding message");
                        self.event_tx
                            .send(ListenerEvent::Message(msg))
                            .await
                            .unwrap();
                    }
                    ServerEvent::KeepAlive { id, .. } => {
                        debug!(id = %id, "received keepalive");
                    }
                    ServerEvent::Open { id, .. } => {
                        debug!(id = %id, "received open event");
                    }
                }
            }

            Ok(())
        }
        .instrument(span)
        .await
    }
}

// Reliable listener implementation
#[derive(Clone)]
pub struct ListenerHandle {
    pub events: async_channel::Receiver<ListenerEvent>,
    pub config: ListenerConfig,
    pub commands: mpsc::Sender<ListenerCommand>,
}

impl ListenerHandle {
    pub fn new(config: ListenerConfig) -> ListenerHandle {
        let (event_tx, event_rx) = async_channel::bounded(64);
        let (commands_tx, commands_rx) = mpsc::channel(1);

        let config_clone = config.clone();

        // use a new local set to isolate panics
        let local_set = LocalSet::new();
        local_set.spawn_local(async move {
            let this = ListenerActor {
                event_tx,
                commands_rx: Some(commands_rx),
                config: config_clone,
                state: ConnectionState::Unitialized,
            };

            this.run_loop().await;
        });
        spawn_local(local_set);

        Self {
            events: event_rx,
            config,
            commands: commands_tx,
        }
    }

    // the response will be sent as an event in self.events
    pub async fn state(&self) -> ConnectionState {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(ListenerCommand::GetState(tx))
            .await
            .unwrap();
        rx.await.unwrap()
    }
}

#[cfg(test)]
mod tests {
    use models::Subscription;
    use serde_json::json;
    use task::LocalSet;

    use crate::http_client::NullableClient;

    use super::*;

    #[tokio::test]
    async fn test_listener_reconnects_on_http_status_500() {
        let local_set = LocalSet::new();
        local_set
            .spawn_local(async {
                let http_client = HttpClient::new_nullable({
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    let nullable = NullableClient::builder()
                        .text_response(url.clone(), 500, "failed")
                        .json_response(url, 200, json!({"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"})).unwrap()
                        .build();
                    nullable
                });
                let credentials = Credentials::new_nullable(vec![]).await.unwrap();

                let config = ListenerConfig {
                    http_client,
                    credentials,
                    endpoint: "http://localhost".to_string(),
                    topic: "test".to_string(),
                    since: 0,
                };

                let listener = ListenerHandle::new(config.clone());
                let items: Vec<_> = listener.events.take(3).collect().await;

                dbg!(&items);
                assert!(matches!(
                    &items[..],
                    &[
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Unitialized),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Reconnecting { .. }),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Connected { .. }),
                    ]
                ));
            });
        local_set.await;
    }

    #[tokio::test]
    async fn test_listener_reconnects_on_invalid_message() {
        let local_set = LocalSet::new();
        local_set
            .spawn_local(async {
                let http_client = HttpClient::new_nullable({
                    let url = Subscription::build_url("http://localhost", "test", 0).unwrap();
                    let nullable = NullableClient::builder()
                        .text_response(url.clone(), 200, "invalid message")
                        .json_response(url, 200, json!({"id":"SLiKI64DOt","time":1635528757,"event":"open","topic":"mytopic"})).unwrap()
                        .build();
                    nullable
                });
                let credentials = Credentials::new_nullable(vec![]).await.unwrap();

                let config = ListenerConfig {
                    http_client,
                    credentials,
                    endpoint: "http://localhost".to_string(),
                    topic: "test".to_string(),
                    since: 0,
                };

                let listener = ListenerHandle::new(config.clone());
                let items: Vec<_> = listener.events.take(3).collect().await;

                dbg!(&items);
                assert!(matches!(
                    &items[..],
                    &[
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Unitialized),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Reconnecting { .. }),
                        ListenerEvent::ConnectionStateChanged(ConnectionState::Connected { .. }),
                    ]
                ));
            });
        local_set.await;
    }
}
