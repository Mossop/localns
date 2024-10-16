#![deny(unreachable_pub)]
mod api;
mod backoff;
mod config;
mod dns;
mod error;
mod sources;
#[cfg(test)]
mod test;
mod util;
mod watcher;

use std::{
    collections::HashMap,
    future::Future,
    mem,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use error::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::{
    api::ApiServer,
    config::Config,
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
    fn add_source_records(&self, new_records: SourceRecords) -> impl Future<Output = ()> + Send;

    fn clear_source_records(&self, source_id: &SourceId) -> impl Future<Output = ()> + Send;
}

#[derive(Clone)]
pub struct Server {
    id: ServerId,
    inner: Arc<Mutex<ServerInner>>,
    sources: Arc<Mutex<Sources>>,
    server_state: Arc<RwLock<ServerState>>,
    dns_server: Arc<Mutex<DnsServer>>,
    config_watcher: LockedOption<Watcher>,
    api_server: LockedOption<ApiServer>,
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
    pub async fn new(config_path: &Path) -> Result<Self, Error> {
        let config = Config::from_file(config_path)?;

        let server_state = Arc::new(RwLock::new(ServerState {
            zones: config.zones.clone(),
            records: RecordSet::new(),
        }));

        let server = Self {
            id: ServerId::new_v4(),
            inner: Arc::new(Mutex::new(ServerInner {
                config: config.clone(),
                records: HashMap::new(),
            })),
            sources: Default::default(),
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
            .and_then(|api_config| ApiServer::new(api_config, server.clone()))
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
        ) {
            Ok(watcher) => {
                server.config_watcher.replace(watcher).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to set up file watcher, config changes will not be detected.");
            }
        }

        Ok(server)
    }

    pub async fn shutdown(&self) {
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

    async fn update_config(&self, config: Config) {
        let (restart_server, restart_api_server, old_config) = {
            let mut server_state = self.server_state.write().await;
            let mut inner = self.inner.lock().await;

            let restart_server = inner.config.server != config.server;
            let restart_api_server = inner.config.api != config.api;

            let mut old_config = config.clone();
            mem::swap(&mut inner.config, &mut old_config);
            server_state.zones = config.zones.clone();

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

            if let Some(api_server) = config
                .api
                .as_ref()
                .and_then(|api_config| ApiServer::new(api_config, self.clone()))
            {
                self.api_server.replace(api_server).await;
            }
        }
    }
}

impl RecordServer for Server {
    async fn add_source_records(&self, new_records: SourceRecords) {
        let mut inner = self.inner.lock().await;

        let mut changed = true;
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

        if changed {
            let records = inner.records();
            let mut server_state = self.server_state.write().await;
            server_state.records = records;
        }
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        let mut inner = self.inner.lock().await;

        if let Some(old) = inner.records.remove(source_id) {
            if !old.records.is_empty() {
                let records = inner.records();
                let mut server_state = self.server_state.write().await;
                server_state.records = records;
            }
        }
    }
}
