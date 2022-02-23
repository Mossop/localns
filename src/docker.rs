use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Context;
use std::time::Duration;

use bollard::models::ContainerSummaryInner;
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

pub type DockerState = Vec<ContainerSummaryInner>;

const TIMEOUT: u64 = 4;

fn check_file(file: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(file).map_err(|e| format!("Failed to read file {}: {}", file.display(), e))?;

    if !metadata.is_file() {
        Err(format!("Expected {} to be a file", file.display()))
    } else {
        Ok(())
    }
}

fn connect(config: &Option<DockerConfig>) -> Result<Docker, String> {
    match config {
        Some(DockerConfig::Address(address)) => {
            if address.starts_with("http://") {
                log::trace!("Attempting to connect to docker daemon over http...");
                Docker::connect_with_http(&address, TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            } else {
                log::trace!("Attempting to connect to docker daemon over local socket...");
                Docker::connect_with_local(&address, TIMEOUT, API_DEFAULT_VERSION)
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
                TIMEOUT,
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
    docker
        .list_containers::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))
}

pin_project! {
    pub struct DockerStateStream {
        #[pin]
        inner: WatchStream<Option<DockerState>>,
    }
}

impl DockerStateStream {
    pub fn new(mut config_stream: Debounced<ConfigStream>) -> Debounced<Self> {
        let (sender, receiver) = watch::channel(None);

        tokio::spawn(async move {
            'config: loop {
                // No config yet
                let mut config = match config_stream.next().await {
                    Some(Some(config)) => config.docker,
                    Some(None) => continue 'config,
                    None => return,
                };

                'docker: loop {
                    let docker = match connect(&config) {
                        Ok(docker) => docker,
                        Err(e) => {
                            log::error!("Failed to connect to docker: {}", e);

                            // Watch for a new config or attempt to reconnect to docker.
                            match timeout(Duration::from_millis(1000), config_stream.next()).await {
                                Ok(Some(Some(new_config))) => {
                                    config = new_config.docker;
                                    // Attempt to reconnect with the new config.
                                    continue 'docker;
                                }
                                Ok(Some(None)) => {
                                    // Lost the config, wait for a new one.
                                    continue 'config;
                                }
                                Ok(None) => {
                                    // Config stream closed. Bail out.
                                    return;
                                }
                                Err(_) => {
                                    // Hit the timeout, try to connect to docker again.
                                    continue 'docker;
                                }
                            }
                        }
                    };

                    match docker.version().await {
                        Ok(version) => match version.version {
                            Some(v) => log::info!("Connected to docker daemon version {}.", v),
                            None => log::info!("Connected to docker daemon."),
                        },
                        Err(e) => {
                            log::error!("Failed to get docker version: {}", e);
                            // Jump back out to wait for a new config on the assumption the docker config
                            // is bad.
                            continue 'config;
                        }
                    }

                    match fetch_state(&docker).await {
                        Ok(state) => {
                            if let Err(e) = sender.send(Some(state)) {
                                log::error!("Failed to send docker state: {}", e);
                                return;
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to collect docker state: {}", e);
                            // Jump back out to wait for a new config on the assumption the docker config
                            // is bad.
                            continue 'config;
                        }
                    };

                    let mut events = docker.events::<&str>(None);
                    loop {
                        select! {
                            ev = events.next() => match ev {
                                Some(Ok(_)) => {
                                    match fetch_state(&docker).await {
                                        Ok(state) => {
                                            if let Err(e) = sender.send(Some(state)) {
                                                log::error!("Failed to send docker state: {}", e);
                                                return;
                                            }
                                        },
                                        Err(e) => {
                                            log::error!("Failed to collect docker state: {}", e);
                                            // Jump back out to wait for a new config on the assumption the docker config
                                            // is bad.
                                            continue 'config;
                                        }
                                    };
                                },
                                Some(Err(e)) => {
                                    log::error!("Docker events stream reported an error: {}", e);
                                    continue 'docker;
                                },
                                None => {
                                    log::trace!("Docker events stream hung up.");
                                    continue 'docker;
                                }
                            },
                            next = config_stream.next() => match next {
                                Some(Some(new_config)) => {
                                    if new_config.docker != config {
                                        config = new_config.docker;
                                        // Attempt to reconnect with the new config.
                                        continue 'docker;
                                    } else {
                                        // Otherwise just wait for more events.
                                        continue;
                                    }
                                },
                                Some(None) => {
                                    // Lost the config, wait for a new one.
                                    continue 'config;
                                },
                                None => {
                                    // Config stream closed. Bail out.
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        });

        Debounced::new(
            DockerStateStream {
                inner: WatchStream::new(receiver),
            },
            Duration::from_millis(1000),
        )
    }
}

impl Stream for DockerStateStream {
    type Item = Option<DockerState>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx)
    }
}
