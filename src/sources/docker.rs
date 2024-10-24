use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context};
use bollard::models;
use bollard::{Docker, API_DEFAULT_VERSION};
use figment::value::magic::RelativePathBuf;
use futures::StreamExt;
use serde::Deserialize;
use tokio::time::sleep;
use tracing::instrument;

use crate::dns::{RData, Record, RecordSet};
use crate::{
    backoff::Backoff,
    sources::{SourceConfig, SourceId, SourceType, SpawnHandle},
    RecordServer, SourceRecords,
};
use crate::{util::Address, Error};

#[derive(Debug, PartialEq, Deserialize, Clone)]
pub(crate) struct DockerLocal {}

#[derive(Debug, PartialEq, Deserialize, Clone)]
pub(crate) struct DockerTls {
    pub address: Address,
    pub private_key: RelativePathBuf,
    pub certificate: RelativePathBuf,
    pub ca: RelativePathBuf,
}

#[derive(Debug, PartialEq, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum DockerConfig {
    Address(String),
    Tls(Box<DockerTls>),
    Local(DockerLocal),
}

type Labels = HashMap<String, String>;

#[derive(Debug, PartialEq, Eq, Clone)]
struct Network {
    id: String,
    name: String,
    driver: Option<String>,
    labels: Labels,
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
struct ContainerEndpoint {
    network: Network,
    ip: Option<Ipv4Addr>,
}

impl ContainerEndpoint {
    fn try_from(
        state: models::EndpointSettings,
        networks: &HashMap<String, Network>,
    ) -> Result<Self, Error> {
        let network_id = state.network_id.context("Missing network id")?;
        let network = networks.get(&network_id).context("Unknown network")?;

        Ok(ContainerEndpoint {
            network: network.clone(),
            ip: state.ip_address.and_then(|s| Ipv4Addr::from_str(&s).ok()),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Container {
    id: String,
    names: Vec<String>,
    image: Option<String>,
    networks: HashMap<String, ContainerEndpoint>,
    labels: Labels,
}

impl Container {
    fn try_from(
        state: models::ContainerSummary,
        networks: &HashMap<String, Network>,
    ) -> Result<Self, Error> {
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
            id: state.id.context("Missing id")?,
            image: state.image,
            names: state.names.unwrap_or_default(),
            networks: container_networks,
            labels: state.labels.unwrap_or_default(),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct DockerState {
    networks: HashMap<String, Network>,
    containers: HashMap<String, Container>,
}

const DOCKER_TIMEOUT: u64 = 4;

fn check_file(file: &Path) -> Result<(), Error> {
    let metadata = fs::metadata(file)?;

    if !metadata.is_file() {
        bail!("Invalid file: '{}'", file.display());
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

#[instrument(fields(%source_id), skip(docker_config))]
fn connect(source_id: &SourceId, docker_config: &DockerConfig) -> Result<Docker, Error> {
    let docker = match docker_config {
        DockerConfig::Address(address) => {
            if address.starts_with("http://") {
                tracing::trace!(address, "Attempting to connect to docker daemon over HTTP");
                Docker::connect_with_http(address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)?
            } else {
                tracing::trace!(address, "Attempting to connect to local docker daemon");
                Docker::connect_with_local(address, DOCKER_TIMEOUT, API_DEFAULT_VERSION)?
            }
        }
        DockerConfig::Local(_) => {
            tracing::trace!("Attempting to connect to local docker daemon");

            Docker::connect_with_local_defaults()?
        }
        DockerConfig::Tls(tls_config) => {
            let private_key = tls_config.private_key.relative();
            check_file(&private_key)?;
            let certificate = tls_config.certificate.relative();
            check_file(&certificate)?;
            let ca = tls_config.ca.relative();
            check_file(&ca)?;

            tracing::trace!(
                address = tls_config.address.address(2376),
                "Attempting to connect to docker daemon over TLS",
            );

            Docker::connect_with_ssl(
                &tls_config.address.address(2376),
                &private_key,
                &certificate,
                &ca,
                DOCKER_TIMEOUT,
                API_DEFAULT_VERSION,
            )?
        }
    };

    Ok(docker)
}

async fn fetch_state(docker: &Docker) -> Result<DockerState, Error> {
    let mut network_state = docker.list_networks::<&str>(None).await?;

    let networks = network_state
        .drain(..)
        .filter_map(|state| {
            let network: Network = state.try_into().ok()?;

            Some((network.id.clone(), network))
        })
        .collect();

    let mut container_state = docker.list_containers::<&str>(None).await?;

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

#[instrument(fields(%source_id), skip(state))]
fn generate_records(source_id: &SourceId, state: DockerState) -> RecordSet {
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
                    tracing::warn!(
                        hostname,
                        "Cannot add record as its 'localns.network' label references an invalid network.",
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

                if let Some(ip) = possible_ips.first() {
                    if possible_ips.len() > 1 {
                        tracing::warn!(
                            hostname,
                            "Cannot add record as it is present on multiple possible networks.",
                        );
                    } else {
                        records.insert(Record::new(hostname.into(), RData::A(*ip)));
                    }
                } else {
                    tracing::warn!(
                        hostname,
                        "Cannot add record as none of its networks appeared usable.",
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

async fn docker_loop<S: RecordServer>(
    source_id: &SourceId,
    docker_config: &DockerConfig,
    server: &S,
) -> LoopResult {
    let docker = match connect(source_id, docker_config) {
        Ok(docker) => docker,
        Err(e) => {
            tracing::error!(%source_id, error=%e, "Error connecting to docker");
            return LoopResult::Backoff;
        }
    };

    let version = match docker.version().await {
        Ok(version) => version,
        Err(e) => {
            tracing::error!(%source_id, error=%e, "Failed to get docker version");
            return LoopResult::Backoff;
        }
    };

    match (version.version, version.api_version) {
        (Some(v), Some(a)) => tracing::debug!(
            %source_id,
            version = v,
            api_version = a,
            "Connected to docker daemon."
        ),
        _ => tracing::debug!(%source_id, "Connected to docker daemon."),
    }

    let state = match fetch_state(&docker).await {
        Ok(state) => state,
        Err(e) => {
            tracing::error!(%source_id, error = %e);
            return LoopResult::Backoff;
        }
    };

    let records = generate_records(source_id, state);
    server
        .add_source_records(SourceRecords::new(source_id, None, records))
        .await;

    let mut events = docker.events::<&str>(None);
    loop {
        match events.next().await {
            Some(Ok(ev)) => {
                if useful_event(&ev) {
                    let state = match fetch_state(&docker).await {
                        Ok(state) => state,
                        Err(e) => {
                            tracing::error!(%source_id, error = %e);
                            return LoopResult::Backoff;
                        }
                    };

                    let records = generate_records(source_id, state);
                    server
                        .add_source_records(SourceRecords::new(source_id, None, records))
                        .await;
                }
            }
            _ => {
                return LoopResult::Retry;
            }
        }
    }
}

impl SourceConfig for DockerConfig {
    type Handle = SpawnHandle;

    fn source_type() -> SourceType {
        SourceType::Docker
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<SpawnHandle, Error> {
        tracing::trace!("Adding source");

        let handle = {
            let source_id = source_id.clone();
            let server = server.clone();
            tokio::spawn(async move {
                let mut backoff = Backoff::default();

                loop {
                    match docker_loop(&source_id, &self, &server).await {
                        LoopResult::Backoff => {
                            server.clear_source_records(&source_id).await;
                            sleep(backoff.next()).await;
                        }
                        LoopResult::Retry => {
                            backoff.reset();
                        }
                    }
                }
            })
        };

        Ok(SpawnHandle { handle })
    }
}
