use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use tokio::task::JoinHandle;

use crate::{config::Config, watcher::Watcher, Error, Server, UniqueId};

mod dhcp;
mod docker;
mod file;
mod remote;
mod traefik;

pub(crate) trait SourceHandle
where
    Self: Send + 'static,
{
    fn source_id(&self) -> &UniqueId;
}

struct SpawnHandle {
    source_id: UniqueId,
    handle: JoinHandle<()>,
}

impl Drop for SpawnHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SourceHandle for SpawnHandle {
    fn source_id(&self) -> &UniqueId {
        &self.source_id
    }
}

struct WatcherHandle {
    source_id: UniqueId,
    _watcher: Watcher,
}

impl SourceHandle for WatcherHandle {
    fn source_id(&self) -> &UniqueId {
        &self.source_id
    }
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
pub(crate) struct SourceConfig {
    #[serde(default)]
    pub(crate) docker: HashMap<String, docker::DockerConfig>,

    #[serde(default)]
    pub traefik: HashMap<String, traefik::TraefikConfig>,

    #[serde(default)]
    pub(crate) dhcp: HashMap<String, dhcp::DhcpConfig>,

    #[serde(default)]
    pub(crate) file: HashMap<String, file::FileConfig>,

    #[serde(default)]
    pub remote: HashMap<String, remote::RemoteConfig>,
}

#[derive(Default)]
pub(crate) struct Sources {
    sources: HashMap<UniqueId, Box<dyn SourceHandle>>,
}

impl Sources {
    pub(crate) async fn install_sources(&mut self, server: &Server, config: &Config) {
        let mut seen_sources: HashSet<UniqueId> = HashSet::new();

        let mut add_source =
            |source: &str, name: &str, result: Result<Box<dyn SourceHandle>, Error>| match result {
                Ok(handle) => {
                    seen_sources.insert(handle.source_id().clone());
                    self.sources.insert(handle.source_id().clone(), handle);
                }
                Err(e) => {
                    tracing::error!(source, name, error=%e, "Failed adding source")
                }
            };

        // DHCP is assumed to not need any additional resolution.
        for (name, dhcp_config) in &config.sources.dhcp {
            add_source(
                "dhcp",
                name,
                dhcp::source(name.clone(), server, dhcp_config).await,
            );
        }

        // File sources are assumed to not need any additional resolution.
        for (name, file_config) in &config.sources.file {
            add_source(
                "file",
                name,
                file::source(name.clone(), server, file_config).await,
            );
        }

        // Docker hostname may depend on DHCP records above.
        for (name, docker_config) in &config.sources.docker {
            add_source(
                "docker",
                name,
                docker::source(name.clone(), server, docker_config).await,
            );
        }

        // Traefik hostname may depend on Docker or DHCP records.
        for (name, traefik_config) in &config.sources.traefik {
            add_source(
                "traefik",
                name,
                traefik::source(name.clone(), server, traefik_config).await,
            );
        }

        // Remote hostname my depend on anything.
        for (name, remote_config) in &config.sources.remote {
            add_source(
                "remote",
                name,
                remote::source(name.clone(), server, remote_config).await,
            );
        }

        let records = {
            let mut inner = server.inner.lock().await;

            let all = self.sources.keys().cloned().collect::<HashSet<UniqueId>>();
            for old in all.difference(&seen_sources) {
                self.sources.remove(old);
                inner.records.remove(old);
            }

            inner.records()
        };

        let mut server_state = server.server_state.write().await;
        server_state.records = records;
    }

    pub(crate) async fn shutdown(&mut self) {
        self.sources.clear();
    }
}
