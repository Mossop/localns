#![deny(unreachable_pub)]
mod api;
mod backoff;
mod config;
mod dns;
mod error;
mod sources;
mod util;
mod watcher;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, Utc};
pub use error::Error;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::{
    api::ApiServer,
    config::Config,
    dns::{DnsServer, RecordSet, ServerState},
    sources::Sources,
    watcher::{watch, WatchListener, Watcher},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct UniqueId {
    id: String,
}

impl UniqueId {
    fn random() -> Self {
        let input_data = [0_u8; 32];

        let digest = Sha256::digest(input_data);

        UniqueId {
            id: hex::encode(digest),
        }
    }

    fn extend(&self, id: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(&self.id);
        digest.update(id);

        UniqueId {
            id: hex::encode(digest.finalize()),
        }
    }
}

struct SourceRecords {
    source_id: UniqueId,
    timestamp: DateTime<Utc>,
    records: RecordSet,
}

impl SourceRecords {
    pub(crate) fn new(
        source_id: &UniqueId,
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

struct ServerInner {
    config: Config,
    records: HashMap<UniqueId, SourceRecords>,
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

#[derive(Clone)]
pub struct Server {
    id: UniqueId,
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
            config: config.clone(),
            records: RecordSet::new(),
        }));

        let server = Self {
            id: UniqueId::random(),
            inner: Arc::new(Mutex::new(ServerInner {
                config: config.clone(),
                records: HashMap::new(),
            })),
            sources: Default::default(),
            dns_server: Arc::new(Mutex::new(DnsServer::new(server_state.clone()).await)),
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
            sources.install_sources(&server, &config).await;
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

    pub(crate) async fn add_source_records(&self, new_records: SourceRecords) {
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

    pub(crate) async fn clear_source_records(&self, source_id: &UniqueId) {
        let mut inner = self.inner.lock().await;

        if let Some(old) = inner.records.remove(source_id) {
            if !old.records.is_empty() {
                let records = inner.records();
                let mut server_state = self.server_state.write().await;
                server_state.records = records;
            }
        }
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
        let (restart_server, restart_api_server) = {
            let mut server_state = self.server_state.write().await;
            let mut inner = self.inner.lock().await;

            let restart_server = inner.config.server != config.server;
            let restart_api_server = inner.config.api != config.api;
            inner.config = config.clone();
            server_state.config = config.clone();

            (restart_server, restart_api_server)
        };

        {
            let mut sources = self.sources.lock().await;
            sources.install_sources(self, &config).await;
        }

        if restart_server {
            let mut dns_server = self.dns_server.lock().await;
            dns_server.restart().await;
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
