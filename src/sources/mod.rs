use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_plain::derive_display_from_serialize;
use tokio::task::JoinHandle;

use crate::{
    config::Config, dns::RecordSet, watcher::Watcher, Error, RecordServer, Server, ServerId,
};

mod dhcp;
mod docker;
mod file;
mod remote;
mod traefik;

trait SourceConfig: PartialEq {
    type Handle: SourceHandle;

    fn source_type() -> SourceType;

    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<Self::Handle, Error>;
}

pub(crate) trait SourceHandle
where
    Self: Send + 'static,
{
}

struct SpawnHandle {
    handle: JoinHandle<()>,
}

impl Drop for SpawnHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SourceHandle for SpawnHandle {}

struct WatcherHandle {
    _watcher: Watcher,
}

impl SourceHandle for WatcherHandle {}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "lowercase")]
pub(crate) enum SourceType {
    File,
    Dhcp,
    Docker,
    Remote,
    Traefik,
}

derive_display_from_serialize!(SourceType);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct SourceId {
    server_id: ServerId,
    source_type: SourceType,
    source_name: String,
}

impl SourceId {
    fn new(server_id: &ServerId, source_type: SourceType, source_name: &str) -> Self {
        Self {
            server_id: *server_id,
            source_type,
            source_name: source_name.to_owned(),
        }
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{},{},{}]",
            self.server_id, self.source_type, self.source_name
        )
    }
}

pub(crate) struct SourceRecords {
    pub(crate) source_id: SourceId,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) records: RecordSet,
}

impl SourceRecords {
    pub(crate) fn new(
        source_id: &SourceId,
        timestamp: Option<DateTime<Utc>>,
        records: RecordSet,
    ) -> Self {
        Self {
            source_id: source_id.clone(),
            timestamp: timestamp.unwrap_or_else(Utc::now),
            records,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
pub(crate) struct SourcesConfig {
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
    sources: HashMap<SourceId, Box<dyn SourceHandle>>,
}

impl Sources {
    async fn add_sources<C>(
        &mut self,
        sources: HashMap<String, C>,
        old_sources: Option<&HashMap<String, C>>,
        server: &Server,
        seen_sources: &mut HashSet<SourceId>,
    ) where
        C: SourceConfig,
    {
        for (name, source_config) in sources {
            let source_id = SourceId::new(&server.id, C::source_type(), &name);
            let previous = old_sources.and_then(|c| c.get(&name));

            seen_sources.insert(source_id.clone());

            if Some(&source_config) != previous {
                self.sources.remove(&source_id);

                match source_config.spawn(source_id.clone(), server).await {
                    Ok(handle) => {
                        self.sources.insert(source_id, Box::new(handle));
                    }
                    Err(e) => {
                        tracing::error!(source = %source_id, error = %e, "Failed adding source")
                    }
                }
            }
        }
    }

    pub(crate) async fn install_sources(
        &mut self,
        server: &Server,
        config: Config,
        old_config: Option<&Config>,
    ) {
        let mut seen_sources: HashSet<SourceId> = HashSet::new();

        // DHCP is assumed to not need any additional resolution.
        self.add_sources(
            config.sources.dhcp,
            old_config.map(|c| &c.sources.dhcp),
            server,
            &mut seen_sources,
        )
        .await;

        // File sources are assumed to not need any additional resolution.
        self.add_sources(
            config.sources.file,
            old_config.map(|c| &c.sources.file),
            server,
            &mut seen_sources,
        )
        .await;

        // Docker hostname may depend on DHCP records above.
        self.add_sources(
            config.sources.docker,
            old_config.map(|c| &c.sources.docker),
            server,
            &mut seen_sources,
        )
        .await;

        // Traefik hostname may depend on Docker or DHCP records.
        self.add_sources(
            config.sources.traefik,
            old_config.map(|c| &c.sources.traefik),
            server,
            &mut seen_sources,
        )
        .await;

        // Remote hostname may depend on anything.
        self.add_sources(
            config.sources.remote,
            old_config.map(|c| &c.sources.remote),
            server,
            &mut seen_sources,
        )
        .await;

        let records = {
            let mut inner = server.inner.lock().await;

            let all = self.sources.keys().cloned().collect::<HashSet<SourceId>>();
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
