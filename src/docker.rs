use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::errors::Error;
use bollard::models::ContainerSummaryInner;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures::future::{AbortHandle, Abortable};
use futures::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio::time::sleep;

use crate::config::DockerConfig;
use crate::Config;

const TIMEOUT: u64 = 4;

fn connect(config: &Option<DockerConfig>) -> Result<Docker, Error> {
    if let Some(ref docker_config) = config {
        if docker_config.address.starts_with("/") {
            Docker::connect_with_local(&docker_config.address, TIMEOUT, API_DEFAULT_VERSION)
        } else if docker_config.address.starts_with("http://") {
            Docker::connect_with_http(&docker_config.address, TIMEOUT, API_DEFAULT_VERSION)
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
            } else {
                panic!("Invalid configuration. Must include ssl keys.")
            }
        }
    } else {
        Docker::connect_with_local_defaults()
    }
}

async fn docker_loop(
    config: &Option<DockerConfig>,
    sender: &Sender<Vec<ContainerSummaryInner>>,
    aborter: &Aborter,
) -> Result<bool, String> {
    log::trace!("Attempting to connect to docker daemon...");
    let docker = connect(config).map_err(|e| format!("Failed to connect to docker: {}", e))?;
    let version = docker
        .version()
        .await
        .map_err(|e| format!("Failed to retrieve docker version: {}", e))?;
    if let Some(daemon_version) = version.version {
        log::trace!("Connected to docker {}.", daemon_version);
    } else {
        log::trace!("Connected to docker.");
    }

    let containers = docker
        .list_containers::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))?;
    log::info!("Retrieved {} containers", containers.len());

    if let Err(e) = sender.send(containers).await {
        log::error!("Failed to send container list: {}", e);
        return Ok(true);
    }

    let mut events = Box::pin(aborter.make_abortable(docker.events::<&str>(None)));
    while let Some(update) = events.next().await {
        match update {
            Ok(response) => {
                log::trace!("Saw docker event {:?}", response);

                let containers = docker
                    .list_containers::<&str>(None)
                    .await
                    .map_err(|e| format!("Failed to list containers: {}", e))?;
                log::info!("Retrieved {} containers", containers.len());

                if let Err(e) = sender.send(containers).await {
                    log::error!("Failed to send container list: {}", e);
                    return Ok(true);
                }
            }
            Err(e) => {
                log::error!("Saw an error in the docker events stream: {}", e);
                return Ok(false);
            }
        }
    }

    Ok(events.is_aborted())
}

struct AbortInner {
    is_aborted: bool,
    abort_handle: Option<AbortHandle>,
}

impl Default for AbortInner {
    fn default() -> AbortInner {
        AbortInner {
            is_aborted: false,
            abort_handle: None,
        }
    }
}

#[derive(Clone, Default)]
struct Aborter {
    inner: Arc<Mutex<AbortInner>>,
}

impl Aborter {
    pub fn new() -> Aborter {
        Default::default()
    }

    pub fn abort(&self) {
        if let Ok(ref mut inner) = self.inner.lock() {
            inner.is_aborted = true;
            if let Some(handle) = inner.abort_handle.take() {
                handle.abort();
            }
        }
    }

    pub fn make_abortable<S>(&self, task: S) -> Abortable<S> {
        let (handle, registration) = AbortHandle::new_pair();

        if let Ok(ref mut inner) = self.inner.lock() {
            if inner.is_aborted {
                handle.abort();
            } else {
                if let Some(old_handle) = inner.abort_handle.replace(handle) {
                    old_handle.abort();
                }
            }
        } else {
            handle.abort();
        }

        return Abortable::new(task, registration);
    }
}

pub struct DockerListener {
    sender: Sender<Vec<ContainerSummaryInner>>,
    abort: Aborter,
}

impl DockerListener {
    pub fn new(config: &Config, sender: Sender<Vec<ContainerSummaryInner>>) -> DockerListener {
        let listener = DockerListener {
            sender,
            abort: Aborter::new(),
        };

        listener.listen(config);
        listener
    }

    pub fn update_config(&mut self, config: &Config) {
        self.abort.abort();
        self.abort = Aborter::new();

        self.listen(config);
    }

    fn listen(&self, config: &Config) {
        let abort = self.abort.clone();
        let docker_config = config.docker.clone();
        let sender = self.sender.clone();

        tokio::spawn(async move {
            loop {
                match docker_loop(&docker_config, &sender, &abort).await {
                    Ok(aborted) => {
                        if aborted {
                            return;
                        }
                    }
                    Err(e) => log::error!("{}", e),
                }

                sleep(Duration::from_millis(500)).await;
            }
        });
    }
}
