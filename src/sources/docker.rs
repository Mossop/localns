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
use tokio::time::sleep;

use crate::dns::{RData, Record, RecordSet};
use crate::util::Address;
use crate::{backoff::Backoff, config::Config};

use super::SourceContext;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DockerLocal {}

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DockerTls {
    pub address: Address,
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
            id: state.id.ok_or_else(|| String::from("Missing id"))?,
            name: state.name.ok_or_else(|| String::from("Missing name"))?,
            driver: state.driver,
            labels: state.labels.unwrap_or_default(),
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
        let network_id = state
            .network_id
            .ok_or_else(|| String::from("Missing network id"))?;
        let network = networks
            .get(&network_id)
            .ok_or_else(|| String::from("Unknown network"))?;

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
        state: models::ContainerSummary,
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
            id: state.id.ok_or_else(|| String::from("Missing id"))?,
            image: state.image,
            names: state.names.unwrap_or_default(),
            networks: container_networks,
            labels: state.labels.unwrap_or_default(),
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

fn useful_event(ev: &models::EventMessage) -> bool {
    if let (Some(models::EventMessageTypeEnum::CONTAINER), Some(action)) =
        (ev.typ, ev.action.as_deref())
    {
        if let Some(pos) = action.find(':') {
            !matches!(&action[..pos], "exec_create" | "exec_start")
        } else {
            !matches!(action, "exec_die")
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
                    "({}) Attempting to connect to docker daemon at {}...",
                    name,
                    address
                );
                Docker::connect_with_http(address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            } else {
                log::trace!(
                    "({}) Attempting to connect to docker daemon at {}...",
                    name,
                    address
                );
                Docker::connect_with_local(address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)
                    .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
            }
        }
        DockerConfig::Local(_) => {
            log::trace!("({}) Attempting to connect to local docker daemon...", name);

            Docker::connect_with_local_defaults()
                .map_err(|e| format!("Failed to connect to docker daemon: {}", e))
        }
        DockerConfig::Tls(tls_config) => {
            let private_key = config.path(&tls_config.private_key);
            check_file(&private_key)?;
            let certificate = config.path(&tls_config.certificate);
            check_file(&certificate)?;
            let ca = config.path(&tls_config.ca);
            check_file(&ca)?;

            log::trace!(
                "({}) Attempting to connect to docker daemon at https://{}/...",
                name,
                tls_config.address.address(2376)
            );

            Docker::connect_with_ssl(
                &tls_config.address.address(2376),
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

fn visible_networks(state: &DockerState) -> HashSet<String> {
    state
        .networks
        .iter()
        .filter_map(|(k, network)| {
            if Some(&"true".to_owned()) == network.labels.get("localns.exposed") {
                Some(k.to_owned())
            } else if let Some(ref driver) = network.driver {
                match driver.as_str() {
                    "host" | "macvlan" | "ipvlan" => Some(k.to_owned()),
                    _ => None,
                }
            } else {
                None
            }
        })
        .collect()
}

fn generate_records(name: &str, state: DockerState) -> RecordSet {
    let mut records = RecordSet::new();

    let networks = visible_networks(&state);

    for container in state.containers.values() {
        if let Some(hostname) = container.labels.get("localns.hostname") {
            if let Some(network) = container.labels.get("localns.network") {
                let mut seen = false;

                for endpoint in container.networks.values() {
                    if &endpoint.network.name == network {
                        if let Some(ip) = endpoint.ip {
                            records.insert(Record::new(hostname.into(), RData::A(ip)));
                            seen = true;
                        }
                    }
                }

                if !seen {
                    log::warn!(
                        "({}) Cannot add record for {} as its 'localns.network' label references an invalid network.",
                        name,
                        hostname
                    )
                }
            } else {
                let possible_ips: Vec<Ipv4Addr> = container
                    .networks
                    .values()
                    .filter_map(|endpoint| {
                        if networks.contains(&endpoint.network.id) {
                            endpoint.ip
                        } else {
                            None
                        }
                    })
                    .collect();

                if let Some(ip) = possible_ips.get(0) {
                    if possible_ips.len() > 1 {
                        log::warn!(
                            "({}) Cannot add record for {} as it is present on multiple possible networks.",
                            name,
                            hostname
                        );
                    } else {
                        records.insert(Record::new(hostname.into(), RData::A(*ip)));
                    }
                } else {
                    log::warn!(
                        "({}) Cannot add record for {} as none of its networks appeared usable.",
                        name,
                        hostname
                    );
                }
            }
        }
    }

    records
}

enum LoopResult {
    Backoff,
    Retry,
}

async fn docker_loop(
    name: &str,
    config: &Config,
    docker_config: &DockerConfig,
    context: &mut SourceContext,
) -> LoopResult {
    let docker = match connect(name, config, docker_config) {
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
        (Some(v), Some(a)) => log::debug!(
            "({}) Connected to docker daemon version {} (API {:?}).",
            name,
            v,
            a
        ),
        _ => log::debug!("({}) Connected to docker daemon.", name),
    }

    let state = match fetch_state(&docker).await {
        Ok(state) => state,
        Err(e) => {
            log::error!("({}) {}", name, e);
            return LoopResult::Backoff;
        }
    };

    let records = generate_records(name, state);
    context.send(records);

    let mut events = docker.events::<&str>(None);
    loop {
        match events.next().await {
            Some(Ok(ev)) => {
                if useful_event(&ev) {
                    let state = match fetch_state(&docker).await {
                        Ok(state) => state,
                        Err(e) => {
                            log::error!("({}) {}", name, e);
                            return LoopResult::Backoff;
                        }
                    };

                    let records = generate_records(name, state);
                    context.send(records);
                }
            }
            _ => {
                return LoopResult::Retry;
            }
        }
    }
}

pub(super) fn source(
    name: String,
    config: Config,
    docker_config: DockerConfig,
    mut context: SourceContext,
) {
    let registration = context.abort_registration();

    tokio::spawn(Abortable::new(
        async move {
            let mut backoff = Backoff::default();

            loop {
                match docker_loop(&name, &config, &docker_config, &mut context).await {
                    LoopResult::Backoff => {
                        context.send(RecordSet::new());
                        sleep(backoff.next()).await;
                    }
                    LoopResult::Retry => {
                        backoff.reset();
                    }
                }
            }
        },
        registration,
    ));
}
