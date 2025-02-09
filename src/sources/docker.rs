use std::{
    collections::{HashMap, HashSet},
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
    str::FromStr,
};

use anyhow::{bail, Context};
use bollard::{models, Docker, API_DEFAULT_VERSION};
use figment::value::magic::RelativePathBuf;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use tracing::{instrument, Span};

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    run_loop::{LoopResult, RunLoop},
    sources::{RecordStore, SourceConfig, SourceHandle, SourceId, SourceType},
    util::Address,
    Error,
};

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
    Local {},
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
    ipv4: Option<Ipv4Addr>,
    ipv6: Option<Ipv6Addr>,
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
            ipv4: state.ip_address.and_then(|s| Ipv4Addr::from_str(&s).ok()),
            ipv6: state
                .global_ipv6_address
                .and_then(|s| Ipv6Addr::from_str(&s).ok()),
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
    matches!(ev.typ, Some(models::EventMessageTypeEnum::CONTAINER))
}

#[instrument(level = "debug", name = "docker_connect", fields(%source_id), skip(docker_config), err)]
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
        DockerConfig::Local {} => {
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

#[instrument(level = "trace", name = "docker_generate_records", fields(%source_id, records), skip(state))]
fn generate_records(source_id: &SourceId, state: DockerState) -> RecordSet {
    let mut records = RecordSet::new();

    let networks = visible_networks(&state);

    for container in state.containers.values() {
        if let Some(hostname) = container.labels.get("localns.hostname") {
            let fqdn = match Fqdn::try_from(hostname.as_str()) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error=%e, hostname, "Error parsing container hostname label");
                    continue;
                }
            };

            if let Some(network) = container.labels.get("localns.network") {
                let mut seen = false;

                for endpoint in container.networks.values() {
                    if &endpoint.network.name == network {
                        if let Some(ip) = endpoint.ipv4 {
                            records.insert(Record::new(fqdn.clone(), RData::A(ip)));
                            seen = true;
                        }

                        if let Some(ip) = endpoint.ipv6 {
                            records.insert(Record::new(fqdn.clone(), RData::Aaaa(ip)));
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
                let mut seen_ip = false;

                for endpoint in container.networks.values() {
                    if networks.contains(&endpoint.network.id) {
                        if let Some(ipv4) = endpoint.ipv4 {
                            seen_ip = true;
                            records.insert(Record::new(fqdn.clone(), RData::A(ipv4)));
                        }

                        if let Some(ipv6) = endpoint.ipv6 {
                            seen_ip = true;
                            records.insert(Record::new(fqdn.clone(), RData::Aaaa(ipv6)));
                        }
                    }
                }

                if !seen_ip {
                    tracing::warn!(
                        hostname,
                        "Cannot add record as none of its networks appeared usable.",
                    );
                }
            }
        }
    }

    let span = Span::current();
    span.record("records", records.len());

    records
}

async fn docker_loop(
    record_store: RecordStore,
    source_id: SourceId,
    docker_config: DockerConfig,
) -> LoopResult {
    let docker = match connect(&source_id, &docker_config) {
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

    let records = generate_records(&source_id, state);
    record_store.add_source_records(&source_id, records).await;

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

                    let records = generate_records(&source_id, state);
                    record_store.add_source_records(&source_id, records).await;
                }
            }
            _ => {
                return LoopResult::Sleep;
            }
        }
    }
}

impl SourceConfig for DockerConfig {
    fn source_type() -> SourceType {
        SourceType::Docker
    }

    async fn spawn(
        self,
        source_id: SourceId,
        record_store: &RecordStore,
        _: &Client,
    ) -> Result<SourceHandle, Error> {
        let handle = {
            let backoff = RunLoop::new(5000);
            let config = self.clone();

            tokio::spawn(
                backoff.run(record_store.clone(), source_id, move |server, source_id| {
                    docker_loop(server, source_id, config.clone())
                }),
            )
        };

        Ok(handle.into())
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use reqwest::Client;
    use testcontainers::{runners::AsyncRunner, GenericImage};

    use crate::{
        dns::RData,
        sources::{docker::DockerConfig, RecordStore, SourceConfig, SourceId},
        test::{fqdn, name},
    };

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn integration() {
        let test_container = GenericImage::new("localns_test_empty", "latest")
            .start()
            .await
            .unwrap();
        let ip = test_container.get_bridge_ip_address().await.unwrap();

        let source_id = SourceId::new(DockerConfig::source_type(), "test");

        let config = DockerConfig::Local {};

        let record_store = RecordStore::new();

        let handle = config
            .spawn(source_id.clone(), &record_store, &Client::new())
            .await
            .unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("test1.home.local.")))
            .await;

        assert_eq!(records.len(), 1);

        match ip {
            IpAddr::V4(ip) => {
                assert!(records.contains(&fqdn("test1.home.local"), &RData::A(ip)));
            }
            IpAddr::V6(ip) => {
                assert!(records.contains(&fqdn("test1.home.local"), &RData::Aaaa(ip)));
            }
        }

        test_container.rm().await.unwrap();

        let records = record_store
            .wait_for_records(|records| !records.has_name(&name("test1.home.local.")))
            .await;

        assert!(records.is_empty());

        let test_container = GenericImage::new("localns_test_empty", "latest")
            .start()
            .await
            .unwrap();
        let ip = test_container.get_bridge_ip_address().await.unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("test1.home.local.")))
            .await;

        assert_eq!(records.len(), 1);

        match ip {
            IpAddr::V4(ip) => {
                assert!(records.contains(&fqdn("test1.home.local"), &RData::A(ip)));
            }
            IpAddr::V6(ip) => {
                assert!(records.contains(&fqdn("test1.home.local"), &RData::Aaaa(ip)));
            }
        }

        handle.drop().await;
    }
}
