use std::time::Duration;

use bollard::models::ContainerSummaryInner;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures::{Stream, StreamExt};
use pin_project_lite::pin_project;
use tokio::select;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_stream::wrappers::WatchStream;

use crate::config::DockerConfig;
use crate::ConfigStream;

pub type DockerState = Vec<ContainerSummaryInner>;

const TIMEOUT: u64 = 4;

fn connect(config: &Option<DockerConfig>) -> Result<Docker, String> {
    log::trace!("Attempting to connect to docker daemon...");

    if let Some(ref docker_config) = config {
        if docker_config.address.starts_with("/") {
            Docker::connect_with_local(&docker_config.address, TIMEOUT, API_DEFAULT_VERSION)
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        } else if docker_config.address.starts_with("http://") {
            Docker::connect_with_http(&docker_config.address, TIMEOUT, API_DEFAULT_VERSION)
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        } else {
            if let (Some(ref pkey), Some(ref cert), Some(ref ca)) = (
                &docker_config.private_key,
                &docker_config.certificate,
                &docker_config.ca,
            ) {
                Docker::connect_with_ssl(
                    &docker_config.address,
                    pkey,
                    cert,
                    ca,
                    TIMEOUT,
                    API_DEFAULT_VERSION,
                )
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            } else {
                Err(format!("Invalid configuration. Must include ssl keys."))
            }
        }
    } else {
        Docker::connect_with_local_defaults()
            .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
    }
}

async fn fetch_state(docker: &Docker) -> Result<DockerState, String> {
    let containers = docker
        .list_containers::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))?;
    log::info!("Retrieved {} containers", containers.len());
    Ok(containers)
}

pin_project! {
    pub struct DockerStateStream {
        #[pin]
        inner: WatchStream<Option<DockerState>>,
    }
}

impl DockerStateStream {
    pub fn new(config: ConfigStream) -> DockerStateStream {
        let (sender, receiver) = watch::channel(None);

        tokio::spawn(async move {
            let mut config_stream = config;

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

        DockerStateStream {
            inner: WatchStream::new(receiver),
        }
    }
}

impl Stream for DockerStateStream {
    type Item = Option<DockerState>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx)
    }
}
