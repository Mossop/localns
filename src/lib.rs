#![deny(unreachable_pub)]
mod api;
mod config;
mod dns;
mod run_loop;
mod sources;
#[cfg(test)]
mod test;
mod util;
mod watcher;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    mem,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as SyncMutex},
};

pub use anyhow::Error;
use chrono::{DateTime, Utc};
use reqwest::Client;
use tokio::sync::Mutex;
use tracing::{instrument, span, Instrument, Level};
use uuid::Uuid;

use crate::{
    api::ApiServer,
    config::{Config, Zones},
    dns::{DnsServer, RecordSet, ServerState},
    sources::{SourceId, SourceRecords, Sources},
    watcher::{watch, WatchListener, Watcher},
};

pub(crate) type ServerId = Uuid;

struct ServerInner {
    config: Config,
    records: HashMap<SourceId, SourceRecords>,
}

impl ServerInner {
    fn records(&self) -> RecordSet {
        self.records
            .values()
            .flat_map(|source| source.records.clone())
            .collect()
    }
}

struct LockedOption<T> {
    inner: Arc<Mutex<Option<T>>>,
}

impl<T> Clone for LockedOption<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Default for LockedOption<T> {
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl<T> LockedOption<T> {
    async fn take(&self) -> Option<T> {
        self.inner.lock().await.take()
    }

    async fn replace(&self, value: T) -> Option<T> {
        self.inner.lock().await.replace(value)
    }
}

pub(crate) trait RecordServer
where
    Self: Send + Sync + Clone + 'static,
{
    type UpdateGuard: Send;

    fn http_client(&self) -> Client;

    fn start_batch_update(&self) -> impl Future<Output = Self::UpdateGuard> + Send;

    fn add_source_records(&self, new_records: SourceRecords) -> impl Future<Output = ()> + Send;

    fn clear_source_records(
        &self,
        source_id: &SourceId,
        timestamp: DateTime<Utc>,
    ) -> impl Future<Output = ()> + Send;

    async fn prune_sources(&self, keep: &HashSet<SourceId>);
}

pub(crate) struct BatchGuard {
    server: Server,
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        let count = {
            let mut count = self.server.batch_count.lock().unwrap();
            *count -= 1;
            *count
        };

        if count == 0 {
            let server = self.server.clone();
            tokio::spawn(async move {
                let inner = server.inner.lock().await;
                server.server_state.replace_records(inner.records()).await;
            });
        }
    }
}

#[derive(Clone)]
pub struct Server {
    batch_count: Arc<SyncMutex<u8>>,
    server_id: ServerId,
    inner: Arc<Mutex<ServerInner>>,
    sources: Arc<Mutex<Sources<Server>>>,
    server_state: ServerState<Zones>,
    dns_server: Arc<Mutex<DnsServer>>,
    config_watcher: LockedOption<Watcher>,
    api_server: LockedOption<ApiServer>,
    http_client: Client,
}

struct ConfigWatcher {
    config_file: PathBuf,
    server: Server,
}

impl WatchListener for ConfigWatcher {
    async fn event(&mut self, _: watcher::FileEvent) {
        match Config::from_file(&self.config_file) {
            Ok(config) => self.server.update_config(config).await,
            Err(e) => {
                tracing::error!(error = %e, "Failed to reload config");
            }
        }
    }
}

impl Server {
    pub fn new(config_path: &Path) -> impl Future<Output = Result<Self, Error>> + '_ {
        let span = span!(Level::TRACE, "server_create");

        async move {
            let config = Config::from_file(config_path)?;

            let server_state = ServerState::new(RecordSet::new(), config.zones.clone());

            let http_client = Client::builder()
                .dns_resolver(Arc::new(server_state.clone()))
                .build()?;

            let sources = Sources::new();
            let server_id = sources.server_id();

            let server = Self {
                http_client,
                batch_count: Default::default(),
                server_id,
                inner: Arc::new(Mutex::new(ServerInner {
                    config: config.clone(),
                    records: HashMap::new(),
                })),
                sources: Arc::new(Mutex::new(sources)),
                dns_server: Arc::new(Mutex::new(
                    DnsServer::new(&config.server, server_state.clone()).await,
                )),
                server_state,
                config_watcher: Default::default(),
                api_server: Default::default(),
            };

            if let Some(api_server) = config
                .api
                .as_ref()
                .and_then(|api_config| ApiServer::new(api_config, server_id, server.inner.clone()))
            {
                server.api_server.replace(api_server).await;
            }

            {
                let mut sources = server.sources.lock().await;
                sources.install_sources(&server, config, None).await;
            }

            match watch(
                config_path,
                ConfigWatcher {
                    config_file: config_path.to_owned(),
                    server: server.clone(),
                },
            )
            .await
            {
                Ok(watcher) => {
                    server.config_watcher.replace(watcher).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to set up file watcher, config changes will not be detected.");
                }
            }

            Ok(server)
        }.instrument(span)
    }

    #[cfg(test)]
    pub(crate) async fn records(&self) -> RecordSet {
        self.server_state.records.read().await.clone()
    }

    #[instrument(name = "server_shutdown", skip(self))]
    pub async fn shutdown(self) {
        tracing::info!("Server shutting down");

        self.config_watcher.take().await;

        if let Some(old_server) = self.api_server.take().await {
            old_server.shutdown().await;
        }

        {
            let mut dns_server = self.dns_server.lock().await;
            dns_server.shutdown().await;
        }

        {
            let mut sources = self.sources.lock().await;
            sources.shutdown().await;
        }
    }

    fn update_config(&self, config: Config) -> impl Future<Output = ()> + '_ {
        let span = span!(Level::TRACE, "update_config");

        async move {
            let (restart_server, restart_api_server, old_config) = {
                let mut inner = self.inner.lock().await;

                let restart_server = inner.config.server != config.server;
                let restart_api_server = inner.config.api != config.api;

                let mut old_config = config.clone();
                mem::swap(&mut inner.config, &mut old_config);
                self.server_state.replace_zones(config.zones.clone()).await;

                (restart_server, restart_api_server, old_config)
            };

            {
                let mut sources = self.sources.lock().await;
                sources
                    .install_sources(self, config.clone(), Some(&old_config))
                    .await;
            }

            if restart_server {
                let mut dns_server = self.dns_server.lock().await;
                dns_server.restart(&config.server).await;
            }

            if restart_api_server {
                if let Some(old_server) = self.api_server.take().await {
                    old_server.shutdown().await;
                }

                if let Some(api_server) = config.api.as_ref().and_then(|api_config| {
                    ApiServer::new(api_config, self.server_id, self.inner.clone())
                }) {
                    self.api_server.replace(api_server).await;
                }
            }
        }
        .instrument(span)
    }
}

impl RecordServer for Server {
    type UpdateGuard = BatchGuard;

    fn http_client(&self) -> Client {
        self.http_client.clone()
    }

    async fn start_batch_update(&self) -> Self::UpdateGuard {
        let _guard = self.inner.lock().await;

        let mut count = self.batch_count.lock().unwrap();
        *count += 1;

        BatchGuard {
            server: self.clone(),
        }
    }

    async fn add_source_records(&self, new_records: SourceRecords) {
        let mut changed = true;
        let mut inner = self.inner.lock().await;

        inner
            .records
            .entry(new_records.source_id.clone())
            .and_modify(|current| {
                if new_records.timestamp < current.timestamp {
                    changed = false;
                    return;
                }

                current.timestamp = new_records.timestamp;
                if new_records.records == current.records {
                    changed = false;
                    return;
                }

                current.records = new_records.records.clone();
            })
            .or_insert(new_records);

        if !changed {
            return;
        }

        let can_update = {
            let batch_count = self.batch_count.lock().unwrap();
            *batch_count == 0
        };

        if can_update {
            self.server_state.replace_records(inner.records()).await;
        }
    }

    async fn clear_source_records(&self, source_id: &SourceId, timestamp: DateTime<Utc>) {
        let mut inner = self.inner.lock().await;

        if let Some(old) = inner.records.get(source_id) {
            if old.timestamp > timestamp {
                return;
            }
        } else {
            return;
        }

        if let Some(old) = inner.records.remove(source_id) {
            if !old.records.is_empty() {
                let can_update = {
                    let batch_count = self.batch_count.lock().unwrap();
                    *batch_count == 0
                };

                if can_update {
                    self.server_state.replace_records(inner.records()).await;
                }
            }
        }
    }

    async fn prune_sources(&self, keep: &HashSet<SourceId>) {
        let mut inner = self.inner.lock().await;

        let all = inner.records.keys().cloned().collect::<HashSet<SourceId>>();
        for old in all.difference(keep) {
            inner.records.remove(old);
        }

        let can_update = {
            let batch_count = self.batch_count.lock().unwrap();
            *batch_count == 0
        };

        if can_update {
            self.server_state.replace_records(inner.records()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        dns::{RData, Record},
        sources::SourceType,
        test::{fqdn, write_file},
    };

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn record_server() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.yml");

        write_file(
            &config_file,
            r#"
server:
  port: 53531
"#,
        )
        .await;

        let server = Server::new(&config_file).await.unwrap();

        let source_id_1 = SourceId::new(&Uuid::new_v4(), SourceType::File, "test");

        let mut records_1 = RecordSet::new();
        records_1.insert(Record::new(
            fqdn("www.example.org"),
            RData::Cname(fqdn("other.example.org")),
        ));

        let now = Utc::now();
        server
            .add_source_records(SourceRecords::new(
                &source_id_1,
                Some(now),
                records_1.clone(),
            ))
            .await;

        let server_records = server.records().await;
        assert_eq!(server_records.len(), 1);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        server
            .add_source_records(SourceRecords::new(
                &source_id_1,
                Some(now),
                records_1.clone(),
            ))
            .await;

        let server_records = server.records().await;
        assert_eq!(server_records.len(), 1);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        records_1.insert(Record::new(
            fqdn("old.example.org"),
            RData::Cname(fqdn("other.example.org")),
        ));

        server
            .add_source_records(SourceRecords::new(
                &source_id_1,
                Some(now - Duration::days(1)),
                records_1.clone(),
            ))
            .await;

        let server_records = server.records().await;
        assert_eq!(server_records.len(), 1);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        let source_id_2 = SourceId::new(&Uuid::new_v4(), SourceType::Docker, "test");

        let mut records_2 = RecordSet::new();
        records_2.insert(Record::new(
            fqdn("other.data.com"),
            RData::Cname(fqdn("www.data.com")),
        ));

        server
            .add_source_records(SourceRecords::new(&source_id_2, None, records_2.clone()))
            .await;

        let server_records = server.records().await;
        assert_eq!(server_records.len(), 2);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));
        assert!(
            server_records.contains(&fqdn("other.data.com"), &RData::Cname(fqdn("www.data.com")))
        );

        let mut keep = HashSet::new();
        keep.insert(source_id_2.clone());
        server.prune_sources(&keep).await;

        let server_records = server.records().await;
        assert_eq!(server_records.len(), 1);
        assert!(
            server_records.contains(&fqdn("other.data.com"), &RData::Cname(fqdn("www.data.com")))
        );

        server.clear_source_records(&source_id_2, Utc::now()).await;

        let server_records = server.records().await;
        assert!(server_records.is_empty());
    }
}
