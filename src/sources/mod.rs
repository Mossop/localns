use std::{
    collections::{HashMap, HashSet},
    fmt,
    mem::forget,
};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_plain::derive_display_from_serialize;
use tokio::task::JoinHandle;
use tracing::instrument;

use crate::{config::Config, dns::store::RecordStore, watcher::Watcher, Error};

pub(crate) mod dhcp;
pub(crate) mod docker;
pub(crate) mod file;
pub(crate) mod remote;
pub(crate) mod traefik;

trait SourceConfig: PartialEq {
    fn source_type() -> SourceType;

    async fn spawn(
        self,
        source_id: SourceId,
        record_store: &RecordStore,
        client: &Client,
    ) -> Result<SourceHandle, Error>;
}

enum SourceHandle {
    Spawned(JoinHandle<()>),
    #[allow(dead_code)]
    Watcher(Watcher),
}

impl From<JoinHandle<()>> for SourceHandle {
    fn from(handle: JoinHandle<()>) -> Self {
        SourceHandle::Spawned(handle)
    }
}

impl From<Watcher> for SourceHandle {
    fn from(watcher: Watcher) -> Self {
        SourceHandle::Watcher(watcher)
    }
}

impl SourceHandle {
    async fn drop(self) {
        if let Self::Spawned(handle) = &self {
            handle.abort();
        }

        forget(self);
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        tracing::warn!("Source handle was not correctly dropped.");
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
    pub(crate) source_type: SourceType,
    pub(crate) source_name: String,
}

impl SourceId {
    pub(crate) fn new(source_type: SourceType, source_name: &str) -> Self {
        Self {
            source_type,
            source_name: source_name.to_owned(),
        }
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{},{}]", self.source_type, self.source_name)
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

pub(crate) struct Sources {
    sources: HashMap<SourceId, SourceHandle>,
    record_store: RecordStore,
    client: Client,
}

impl Sources {
    pub(crate) fn new(record_store: RecordStore, client: Client) -> Self {
        Self {
            sources: HashMap::new(),
            record_store,
            client,
        }
    }

    async fn list_sources<C>(
        &mut self,
        sources: &HashMap<String, C>,
        seen_sources: &mut HashSet<SourceId>,
    ) where
        C: SourceConfig,
    {
        for name in sources.keys() {
            let source_id = SourceId::new(C::source_type(), name);
            seen_sources.insert(source_id.clone());
        }
    }

    #[instrument(level = "debug", skip_all, fields(source_type = %C::source_type()))]
    async fn spawn_sources<C>(
        &mut self,
        sources: HashMap<String, C>,
        old_sources: Option<&HashMap<String, C>>,
    ) where
        C: SourceConfig,
    {
        for (name, source_config) in sources {
            tracing::debug!(name, source_type=%C::source_type(), "Adding source");
            let source_id = SourceId::new(C::source_type(), &name);
            let previous = old_sources.and_then(|c| c.get(&name));

            if Some(&source_config) != previous {
                if let Some(handle) = self.sources.remove(&source_id) {
                    handle.drop().await;
                }

                match source_config
                    .spawn(source_id.clone(), &self.record_store, &self.client)
                    .await
                {
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

    #[instrument(level = "debug", skip_all)]
    pub(crate) async fn install_sources(&mut self, config: Config, old_config: Option<&Config>) {
        {
            // First enumerate the configured sources and drop those that are no longer present.
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

            self.record_store.prune_sources(&seen_sources).await;
        }

        // Now install the new sources.

        // DHCP is assumed to not need any additional resolution.
        self.spawn_sources(config.sources.dhcp, old_config.map(|c| &c.sources.dhcp))
            .await;

        // File sources are assumed to not need any additional resolution.
        self.spawn_sources(config.sources.file, old_config.map(|c| &c.sources.file))
            .await;

        // Docker hostname may depend on DHCP records above.
        self.spawn_sources(config.sources.docker, old_config.map(|c| &c.sources.docker))
            .await;

        // Traefik hostname may depend on Docker or DHCP records.
        self.spawn_sources(
            config.sources.traefik,
            old_config.map(|c| &c.sources.traefik),
        )
        .await;

        // Remote hostname may depend on anything.
        self.spawn_sources(config.sources.remote, old_config.map(|c| &c.sources.remote))
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

    use reqwest::Client;
    use tempfile::TempDir;

    use crate::{
        config::Config,
        dns::RData,
        sources::{RecordStore, Sources},
        test::{fqdn, name, write_file},
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
www.test.local:
    type: CNAME
    value: home.test.local
"#,
        )
        .await;

        let zone_2 = temp.path().join("zone2.yml");
        write_file(
            &zone_2,
            r#"
home.other.local: 10.45.23.57
www.other.local:
    type: CNAME
    value: home.other.local
test.other.local:
    type: CNAME
    value: home.test.local
"#,
        )
        .await;

        let zone_3 = temp.path().join("zone3.yml");
        write_file(
            &zone_3,
            r#"
foo.baz.local:
    type: CNAME
    value: home.other.local
"#,
        )
        .await;

        let record_store = RecordStore::new();
        let mut sources = Sources::new(record_store.clone(), Client::new());

        let config_1 = Config::from_file(&config_file).unwrap();

        sources.install_sources(config_1.clone(), None).await;

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("home.test.local.")))
            .await;

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
            .install_sources(config_2.clone(), Some(&config_1))
            .await;

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("www.other.local.")))
            .await;

        assert_eq!(records.len(), 5);

        assert!(records.contains(
            &fqdn("home.test.local"),
            &RData::A(Ipv4Addr::from_str("10.45.23.56").unwrap())
        ));
        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::Cname(fqdn("home.test.local"))
        ));

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
            .install_sources(config_3.clone(), Some(&config_2))
            .await;

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("foo.baz.local.")))
            .await;

        assert_eq!(records.len(), 4);

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

        assert!(records.contains(
            &fqdn("foo.baz.local"),
            &RData::Cname(fqdn("home.other.local"))
        ));

        write_file(&config_file, "").await;
        let config_4 = Config::from_file(&config_file).unwrap();
        sources
            .install_sources(config_4.clone(), Some(&config_3))
            .await;

        record_store
            .wait_for_records(|records| records.is_empty())
            .await;
    }
}
