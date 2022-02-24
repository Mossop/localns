use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bollard::models::SystemEventsResponse;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures::{Stream, StreamExt};
use pin_project_lite::pin_project;
use serde::Deserialize;
use tokio::select;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_stream::wrappers::WatchStream;

use crate::debounce::Debounced;
use crate::ConfigStream;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DockerTls {
    pub address: String,
    pub private_key: PathBuf,
    pub certificate: PathBuf,
    pub ca: PathBuf,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
#[serde(untagged)]
pub enum DockerConfig {
    Address(String),
    Tls(DockerTls),
}

pub type Labels = HashMap<String, String>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub containers: Vec<String>,
    pub labels: Labels,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Container {
    pub id: String,
    pub names: Vec<String>,
    pub image: String,
    pub networks: Vec<String>,
    pub labels: Labels,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DockerState {
    pub networks: HashMap<String, Network>,
    pub containers: HashMap<String, Container>,
}

const DOCKER_TIMEOUT: u64 = 4;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const STATE_DEBOUNCE: Duration = Duration::from_millis(500);

fn check_file(file: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(file).map_err(|e| format!("Failed to read file {}: {}", file.display(), e))?;

    if !metadata.is_file() {
        Err(format!("Expected {} to be a file", file.display()))
    } else {
        Ok(())
    }
}

fn useful_event(ev: &SystemEventsResponse) -> bool {
    if let (Some(typ), Some(action)) = (ev.typ.as_deref(), ev.action.as_deref()) {
        match typ {
            "container" => {
                if let Some(pos) = action.find(':') {
                    match &action[..pos] {
                        "exec_create" | "exec_start" => false,
                        _ => true,
                    }
                } else {
                    match action {
                        "exec_die" => false,
                        _ => true,
                    }
                }
            }
            _ => true,
        }
    } else {
        true
    }
}

fn connect(config: &Option<DockerConfig>) -> Result<Docker, String> {
    match config {
        Some(DockerConfig::Address(address)) => {
            if address.starts_with("http://") {
                log::trace!("Attempting to connect to docker daemon over http...");
                Docker::connect_with_http(&address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            } else {
                log::trace!("Attempting to connect to docker daemon over local socket...");
                Docker::connect_with_local(&address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            }
        }
        Some(DockerConfig::Tls(tls_config)) => {
            log::trace!("Attempting to connect to docker daemon over TLS...");

            check_file(&tls_config.private_key)?;
            check_file(&tls_config.certificate)?;
            check_file(&tls_config.ca)?;

            Docker::connect_with_ssl(
                &tls_config.address,
                &tls_config.private_key,
                &tls_config.certificate,
                &tls_config.ca,
                DOCKER_TIMEOUT,
                API_DEFAULT_VERSION,
            )
            .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        }
        None => {
            log::trace!("Attempting to connect to local docker daemon...");

            Docker::connect_with_local_defaults()
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        }
    }
}

async fn fetch_state(docker: &Docker) -> Result<DockerState, String> {
    let mut container_state = docker
        .list_containers::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))?;

    let containers = container_state
        .drain(..)
        .filter_map(|state| {
            let id = state.id?;

            Some((
                id.clone(),
                Container {
                    id,
                    image: state.image?,
                    names: state.names?,
                    networks: state.network_settings?.networks?.keys().cloned().collect(),
                    labels: state.labels?,
                },
            ))
        })
        .collect();

    let mut network_state = docker
        .list_networks::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list networks: {}", e))?;

    let networks = network_state
        .drain(..)
        .filter_map(|state| {
            let id = state.id?;

            Some((
                id.clone(),
                Network {
                    id,
                    name: state.name?,
                    containers: state.containers?.keys().cloned().collect(),
                    labels: state.labels?,
                },
            ))
        })
        .collect();

    Ok(DockerState {
        networks,
        containers,
    })
}

async fn docker_loop(
    config_stream: &mut ConfigStream,
    sender: &watch::Sender<Option<DockerState>>,
) -> Result<(), String> {
    let config = config_stream.config.docker.clone();
    let docker = connect(&config)?;

    let version = docker
        .version()
        .await
        .map_err(|e| format!("Failed to get docker version: {}", e))?;
    match (version.version, version.api_version) {
        (Some(v), Some(a)) => log::info!("Connected to docker daemon version {} (API {:?}).", v, a),
        _ => log::info!("Connected to docker daemon."),
    }

    let state = fetch_state(&docker).await?;
    sender
        .send(Some(state))
        .map_err(|e| format!("Failed to send docker state: {}", e))?;

    let mut events = docker.events::<&str>(None);
    loop {
        select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    if useful_event(&ev) {
                        log::trace!("Saw docker event {:?} {:?}", ev.typ, ev.action);
                        let state = fetch_state(&docker).await?;
                        sender
                            .send(Some(state))
                            .map_err(|e| format!("Failed to send docker state: {}", e))?;
                    }
                },
                Some(Err(e)) => {
                    log::error!("Docker events stream reported an error: {}", e);
                    return Ok(());
                },
                None => {
                    log::trace!("Docker events stream hung up.");
                    return Ok(());
                }
            },
            next = config_stream.next() => match next {
                Some(new_config) => {
                    if new_config.docker != config {
                        // New configuration so reconnect with it.
                        return Ok(());
                    } else {
                        // Otherwise just wait for more events.
                        continue;
                    }
                },
                None => {
                    // Config stream closed. Bail out.
                    return Err(format!("Configuration stream closed."));
                }
            }
        }
    }
}

pin_project! {
    pub struct DockerStateStream {
        #[pin]
        inner: Debounced<WatchStream<Option<DockerState>>>,
    }
}

impl DockerStateStream {
    pub fn new(mut config_stream: ConfigStream) -> Self {
        let (sender, receiver) = watch::channel(None);

        tokio::spawn(async move {
            loop {
                if let Err(e) = docker_loop(&mut config_stream, &sender).await {
                    log::error!("{}", e);

                    // Attempt to reconnect after a delay but immediately if a new config is
                    // available.
                    if let Ok(None) = timeout(RECONNECT_DELAY, config_stream.next()).await {
                        // Config stream timed out, give up.
                        return;
                    }
                }
            }
        });

        DockerStateStream {
            inner: Debounced::new(WatchStream::new(receiver), STATE_DEBOUNCE.clone()),
        }
    }
}

impl Stream for DockerStateStream {
    type Item = Option<DockerState>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx)
    }
}
