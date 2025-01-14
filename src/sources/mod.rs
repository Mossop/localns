use std::{
    collections::{HashMap, HashSet},
    fmt,
    mem::forget,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_plain::derive_display_from_serialize;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use crate::{config::Config, dns::RecordSet, watcher::Watcher, Error, RecordServer, ServerId};

pub(crate) mod dhcp;
pub(crate) mod docker;
pub(crate) mod file;
pub(crate) mod remote;
pub(crate) mod traefik;

trait SourceConfig: PartialEq {
    fn source_type() -> SourceType;

    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<SourceHandle<S>, Error>;
}

enum SourceHandle<S: RecordServer> {
    Spawned(JoinHandle<()>),
    #[allow(dead_code)]
    Watcher(Watcher),
    Remote(remote::RemoteRecords<S>),
}

impl<S: RecordServer> From<remote::RemoteRecords<S>> for SourceHandle<S> {
    fn from(handle: remote::RemoteRecords<S>) -> Self {
        SourceHandle::Remote(handle)
    }
}

impl<S: RecordServer> From<JoinHandle<()>> for SourceHandle<S> {
    fn from(handle: JoinHandle<()>) -> Self {
        SourceHandle::Spawned(handle)
    }
}

impl<S: RecordServer> From<Watcher> for SourceHandle<S> {
    fn from(watcher: Watcher) -> Self {
        SourceHandle::Watcher(watcher)
    }
}

impl<S: RecordServer> SourceHandle<S> {
    async fn drop(mut self) {
        match &mut self {
            Self::Spawned(handle) => handle.abort(),
            Self::Remote(records) => records.drop().await,
            _ => {}
        }

        forget(self);
    }
}

impl<S: RecordServer> Drop for SourceHandle<S> {
    fn drop(&mut self) {
        warn!("Source handle was not correctly dropped.");
        #[cfg(test)]
        panic!("Source handle was not correctly dropped.");
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SourceType {
    File,
    Dhcp,
    Docker,
    Remote,
    Traefik,
}

derive_display_from_serialize!(SourceType);

#[derive(Debug, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct SourceId {
    #[serde(with = "uuid::serde::braced")]
    pub(crate) server_id: ServerId,
    pub(crate) source_type: SourceType,
    pub(crate) source_name: String,
}

impl SourceId {
    pub(crate) fn new(server_id: &ServerId, source_type: SourceType, source_name: &str) -> Self {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

pub(crate) struct Sources<S: RecordServer> {
    server_id: ServerId,
    sources: HashMap<SourceId, SourceHandle<S>>,
}

impl<S: RecordServer> Sources<S> {
    pub(crate) fn new() -> Self {
        Self {
            server_id: Uuid::new_v4(),
            sources: HashMap::new(),
        }
    }

    pub(crate) fn server_id(&self) -> ServerId {
        self.server_id
    }

    async fn list_sources<C>(
        &mut self,
        sources: &HashMap<String, C>,
        seen_sources: &mut HashSet<SourceId>,
    ) where
        C: SourceConfig,
    {
        for name in sources.keys() {
            let source_id = SourceId::new(&self.server_id, C::source_type(), name);
            seen_sources.insert(source_id.clone());
        }
    }

    async fn spawn_sources<C>(
        &mut self,
        sources: HashMap<String, C>,
        old_sources: Option<&HashMap<String, C>>,
        server: &S,
    ) where
        C: SourceConfig,
    {
        for (name, source_config) in sources {
            tracing::debug!(name, source_type=%C::source_type(), "Adding source");
            let source_id = SourceId::new(&self.server_id, C::source_type(), &name);
            let previous = old_sources.and_then(|c| c.get(&name));

            if Some(&source_config) != previous {
                let _guard = server.start_batch_update().await;

                if let Some(handle) = self.sources.remove(&source_id) {
                    handle.drop().await;
                }

                match source_config.spawn(source_id.clone(), server).await {
                    Ok(handle) => {
                        self.sources.insert(source_id, handle);
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
        server: &S,
        config: Config,
        old_config: Option<&Config>,
    ) {
        {
            // First enumerate the configured sources and drop those that are no longer present.
            let _guard = server.start_batch_update().await;

            let mut seen_sources: HashSet<SourceId> = HashSet::new();

            self.list_sources(&config.sources.dhcp, &mut seen_sources)
                .await;
            self.list_sources(&config.sources.file, &mut seen_sources)
                .await;
            self.list_sources(&config.sources.docker, &mut seen_sources)
                .await;
            self.list_sources(&config.sources.traefik, &mut seen_sources)
                .await;
            self.list_sources(&config.sources.remote, &mut seen_sources)
                .await;

            let all = self.sources.keys().cloned().collect::<HashSet<SourceId>>();
            for old in all.difference(&seen_sources) {
                if let Some(handle) = self.sources.remove(old) {
                    handle.drop().await;
                }
            }

            server.prune_sources(&seen_sources).await;
        }

        // Now install the new sources.

        // DHCP is assumed to not need any additional resolution.
        self.spawn_sources(
            config.sources.dhcp,
            old_config.map(|c| &c.sources.dhcp),
            server,
        )
        .await;

        // File sources are assumed to not need any additional resolution.
        self.spawn_sources(
            config.sources.file,
            old_config.map(|c| &c.sources.file),
            server,
        )
        .await;

        // Docker hostname may depend on DHCP records above.
        self.spawn_sources(
            config.sources.docker,
            old_config.map(|c| &c.sources.docker),
            server,
        )
        .await;

        // Traefik hostname may depend on Docker or DHCP records.
        self.spawn_sources(
            config.sources.traefik,
            old_config.map(|c| &c.sources.traefik),
            server,
        )
        .await;

        // Remote hostname may depend on anything.
        self.spawn_sources(
            config.sources.remote,
            old_config.map(|c| &c.sources.remote),
            server,
        )
        .await;
    }

    pub(crate) async fn shutdown(&mut self) {
        for (_, source_handle) in self.sources.drain() {
            source_handle.drop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

    use tempfile::TempDir;

    use crate::{
        config::Config,
        dns::RData,
        sources::{SourceId, SourceType, Sources},
        test::{fqdn, name, write_file, MultiSourceServer},
    };

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let temp = TempDir::new().unwrap();
        let config_file = temp.path().join("config.yml");
        write_file(
            &config_file,
            r#"
sources:
  file:
    test_file: zone1.yml
"#,
        )
        .await;

        let zone_1 = temp.path().join("zone1.yml");
        write_file(
            &zone_1,
            r#"
home.test.local: 10.45.23.56
www.test.local: home.test.local
"#,
        )
        .await;

        let zone_2 = temp.path().join("zone2.yml");
        write_file(
            &zone_2,
            r#"
home.other.local: 10.45.23.57
www.other.local: home.other.local
test.other.local: home.test.local
"#,
        )
        .await;

        let zone_3 = temp.path().join("zone3.yml");
        write_file(
            &zone_3,
            r#"
foo.baz.local: home.other.local
"#,
        )
        .await;

        let mut sources = Sources::new();
        let mut test_server = MultiSourceServer::new();

        let source_id_1 = SourceId::new(&sources.server_id, SourceType::File, "test_file");
        let source_id_2 = SourceId::new(&sources.server_id, SourceType::File, "other_file");
        let source_id_3 = SourceId::new(&sources.server_id, SourceType::File, "new_file");

        let config_1 = Config::from_file(&config_file).unwrap();

        sources
            .install_sources(&test_server, config_1.clone(), None)
            .await;

        let record_map = test_server
            .wait_for_records(|records| records.has_name(&name("home.test.local.")))
            .await;

        assert_eq!(record_map.len(), 1);
        assert!(record_map.contains_key(&source_id_1));

        let records = record_map.get(&source_id_1).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.contains(
            &fqdn("home.test.local"),
            &RData::A(Ipv4Addr::from_str("10.45.23.56").unwrap())
        ));
        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::Cname(fqdn("home.test.local"))
        ));

        write_file(
            &config_file,
            r#"
sources:
  file:
    test_file: zone1.yml
    other_file: zone2.yml
"#,
        )
        .await;

        let config_2 = Config::from_file(&config_file).unwrap();
        sources
            .install_sources(&test_server, config_2.clone(), Some(&config_1))
            .await;

        let record_map = test_server
            .wait_for_records(|records| records.has_name(&name("www.other.local.")))
            .await;

        assert_eq!(record_map.len(), 2);
        assert!(record_map.contains_key(&source_id_1));
        assert!(record_map.contains_key(&source_id_2));

        let records = record_map.get(&source_id_1).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.contains(
            &fqdn("home.test.local"),
            &RData::A(Ipv4Addr::from_str("10.45.23.56").unwrap())
        ));
        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::Cname(fqdn("home.test.local"))
        ));

        let records = record_map.get(&source_id_2).unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.contains(
            &fqdn("home.other.local"),
            &RData::A(Ipv4Addr::from_str("10.45.23.57").unwrap())
        ));
        assert!(records.contains(
            &fqdn("www.other.local"),
            &RData::Cname(fqdn("home.other.local"))
        ));
        assert!(records.contains(
            &fqdn("test.other.local"),
            &RData::Cname(fqdn("home.test.local"))
        ));

        write_file(
            &config_file,
            r#"
sources:
  file:
    other_file: zone2.yml
    new_file: zone3.yml
"#,
        )
        .await;

        let config_3 = Config::from_file(&config_file).unwrap();
        sources
            .install_sources(&test_server, config_3.clone(), Some(&config_2))
            .await;

        let record_map = test_server
            .wait_for_records(|records| records.has_name(&name("foo.baz.local.")))
            .await;

        assert_eq!(record_map.len(), 2);
        assert!(record_map.contains_key(&source_id_2));
        assert!(record_map.contains_key(&source_id_3));

        let records = record_map.get(&source_id_2).unwrap();
        assert_eq!(records.len(), 3);
        assert!(records.contains(
            &fqdn("home.other.local"),
            &RData::A(Ipv4Addr::from_str("10.45.23.57").unwrap())
        ));
        assert!(records.contains(
            &fqdn("www.other.local"),
            &RData::Cname(fqdn("home.other.local"))
        ));
        assert!(records.contains(
            &fqdn("test.other.local"),
            &RData::Cname(fqdn("home.test.local"))
        ));

        let records = record_map.get(&source_id_3).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records.contains(
            &fqdn("foo.baz.local"),
            &RData::Cname(fqdn("home.other.local"))
        ));

        write_file(&config_file, "").await;
        let config_4 = Config::from_file(&config_file).unwrap();
        sources
            .install_sources(&test_server, config_4.clone(), Some(&config_3))
            .await;

        let state = test_server.wait_for_change().await;
        assert!(state.is_empty());
        assert!(sources.sources.is_empty());
    }
}
