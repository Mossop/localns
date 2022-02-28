use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use bollard::models;
use bollard::{Docker, API_DEFAULT_VERSION};
use futures::future::Abortable;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::backoff::Backoff;
use crate::rfc1035::{AbsoluteName, Record, RecordData};
use crate::Config;

use super::{create_source, RecordSet, RecordSource};

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DockerLocal {}

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
    Local(DockerLocal),
}

pub type Labels = HashMap<String, String>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: Option<String>,
    pub labels: Labels,
}

impl TryFrom<models::Network> for Network {
    type Error = String;

    fn try_from(state: models::Network) -> Result<Self, Self::Error> {
        Ok(Network {
            id: state.id.ok_or(String::from("Missing id"))?,
            name: state.name.ok_or(String::from("Missing name"))?,
            driver: state.driver,
            labels: state.labels.unwrap_or(HashMap::new()),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ContainerEndpoint {
    pub network: Network,
    pub ip: Option<Ipv4Addr>,
}

impl ContainerEndpoint {
    fn try_from(
        state: models::EndpointSettings,
        networks: &HashMap<String, Network>,
    ) -> Result<Self, String> {
        let network_id = state.network_id.ok_or(String::from("Missing network id"))?;
        let network = networks
            .get(&network_id)
            .ok_or(String::from("Unknown network"))?;

        Ok(ContainerEndpoint {
            network: network.clone(),
            ip: state.ip_address.and_then(|s| Ipv4Addr::from_str(&s).ok()),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Container {
    pub id: String,
    pub names: Vec<String>,
    pub image: Option<String>,
    pub networks: HashMap<String, ContainerEndpoint>,
    pub labels: Labels,
}

impl Container {
    fn try_from(
        state: models::ContainerSummaryInner,
        networks: &HashMap<String, Network>,
    ) -> Result<Self, String> {
        let container_networks = match state.network_settings {
            Some(settings) => match settings.networks {
                Some(mut endpoints) => endpoints
                    .drain()
                    .filter_map(|(_, state)| ContainerEndpoint::try_from(state, networks).ok())
                    .map(|n| (n.network.id.clone(), n))
                    .collect(),
                None => HashMap::new(),
            },
            None => HashMap::new(),
        };

        Ok(Container {
            id: state.id.ok_or(String::from("Missing id"))?,
            image: state.image,
            names: state.names.unwrap_or(Vec::new()),
            networks: container_networks,
            labels: state.labels.unwrap_or(HashMap::new()),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DockerState {
    pub networks: HashMap<String, Network>,
    pub containers: HashMap<String, Container>,
}

const DOCKER_TIMEOUT: u64 = 4;

fn check_file(file: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(file).map_err(|e| format!("Failed to read file {}: {}", file.display(), e))?;

    if !metadata.is_file() {
        Err(format!("Expected {} to be a file", file.display()))
    } else {
        Ok(())
    }
}

fn useful_event(ev: &models::SystemEventsResponse) -> bool {
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

fn connect(name: &str, config: &Config, docker_config: &DockerConfig) -> Result<Docker, String> {
    match docker_config {
        DockerConfig::Address(address) => {
            if address.starts_with("http://") {
                log::trace!(
                    "({}) Attempting to connect to docker daemon over http...",
                    name
                );
                Docker::connect_with_http(&address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            } else {
                log::trace!(
                    "({}) Attempting to connect to docker daemon over local socket...",
                    name
                );
                Docker::connect_with_local(&address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            }
        }
        DockerConfig::Local(_) => {
            log::trace!("({}) Attempting to connect to local docker daemon...", name);

            Docker::connect_with_local_defaults()
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        }
        DockerConfig::Tls(tls_config) => {
            log::trace!(
                "({}) Attempting to connect to docker daemon over TLS...",
                name
            );

            let private_key = config.path(&tls_config.private_key);
            check_file(&private_key)?;
            let certificate = config.path(&tls_config.certificate);
            check_file(&certificate)?;
            let ca = config.path(&tls_config.ca);
            check_file(&ca)?;

            Docker::connect_with_ssl(
                &tls_config.address,
                &private_key,
                &certificate,
                &ca,
                DOCKER_TIMEOUT,
                API_DEFAULT_VERSION,
            )
            .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        }
    }
}

async fn fetch_state(docker: &Docker) -> Result<DockerState, String> {
    let mut network_state = docker
        .list_networks::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list networks: {}", e))?;

    let networks = network_state
        .drain(..)
        .filter_map(|state| {
            let network: Network = state.try_into().ok()?;

            Some((network.id.clone(), network))
        })
        .collect();

    let mut container_state = docker
        .list_containers::<&str>(None)
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))?;

    let containers = container_state
        .drain(..)
        .filter_map(|state| {
            let container = Container::try_from(state, &networks).ok()?;

            Some((container.id.clone(), container))
        })
        .collect();

    Ok(DockerState {
        networks,
        containers,
    })
}

fn generate_records(state: DockerState) -> RecordSet {
    let mut records = HashSet::new();

    for container in state.containers.values() {
        if let Some(hostname) = container.labels.get("docker-dns.hostname") {
            records.insert(Record {
                name: AbsoluteName::new(hostname),
                data: RecordData::Cname(AbsoluteName::new("foo")),
            });
        }
    }

    records
}

enum LoopResult {
    Backoff,
    Retry,
    Quit,
}

async fn docker_loop(
    name: &str,
    config: &Config,
    docker_config: &DockerConfig,
    sender: &mpsc::Sender<RecordSet>,
) -> LoopResult {
    let docker = match connect(&name, config, docker_config) {
        Ok(docker) => docker,
        Err(e) => {
            log::error!("({}) {}", name, e);
            return LoopResult::Backoff;
        }
    };

    let version = match docker.version().await {
        Ok(version) => version,
        Err(e) => {
            log::error!("({}) Failed to get docker version: {}", name, e);
            return LoopResult::Backoff;
        }
    };

    match (version.version, version.api_version) {
        (Some(v), Some(a)) => log::info!(
            "({}) Connected to docker daemon version {} (API {:?}).",
            name,
            v,
            a
        ),
        _ => log::info!("({}) Connected to docker daemon.", name),
    }

    let state = match fetch_state(&docker).await {
        Ok(state) => state,
        Err(e) => {
            log::error!("({}) {}", name, e);
            return LoopResult::Backoff;
        }
    };

    let records = generate_records(state);
    if let Err(_) = sender
        .send(records)
        .await
        .map_err(|e| format!("Failed to send records: {}", e))
    {
        return LoopResult::Quit;
    }

    let mut events = docker.events::<&str>(None);
    loop {
        match events.next().await {
            Some(Ok(ev)) => {
                if useful_event(&ev) {
                    log::trace!("({}) Saw docker event {:?} {:?}", name, ev.typ, ev.action);
                    let state = match fetch_state(&docker).await {
                        Ok(state) => state,
                        Err(e) => {
                            log::error!("({}) {}", name, e);
                            return LoopResult::Backoff;
                        }
                    };

                    let records = generate_records(state);
                    if let Err(_) = sender
                        .send(records)
                        .await
                        .map_err(|e| format!("Failed to send records: {}", e))
                    {
                        return LoopResult::Quit;
                    }
                }
            }
            Some(Err(e)) => {
                log::error!("({}) Docker events stream reported an error: {}", name, e);
                return LoopResult::Retry;
            }
            None => {
                log::trace!("({}) Docker events stream hung up.", name);
                return LoopResult::Retry;
            }
        }
    }
}

pub(super) fn docker_source(
    name: String,
    config: Config,
    docker_config: DockerConfig,
) -> RecordSource {
    let (sender, registration, source) = create_source();

    tokio::spawn(Abortable::new(
        async move {
            let mut backoff = Backoff::default();

            loop {
                match docker_loop(&name, &config, &docker_config, &sender).await {
                    LoopResult::Backoff => {
                        if let Err(_) = sender.send(HashSet::new()).await {
                            return;
                        }

                        sleep(backoff.next()).await;
                    }
                    LoopResult::Retry => {
                        backoff.reset();
                    }
                    LoopResult::Quit => {
                        return;
                    }
                }
            }
        },
        registration,
    ));

    source
}
