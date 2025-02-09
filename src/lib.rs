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
    mem,
    ops::DerefMut,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use anyhow::Error;
use reqwest::Client;
use tokio::sync::Mutex;
use tracing::instrument;
use uuid::Uuid;

use crate::{
    api::ApiServer,
    config::{Config, Zones},
    dns::{store::RecordStore, DnsServer, ServerState},
    sources::Sources,
    watcher::{watch, WatchListener, Watcher},
};

pub(crate) type ServerId = Uuid;

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
    config: Arc<Mutex<Config>>,
    sources: Arc<Mutex<Sources>>,
    server_state: ServerState<Zones>,
    record_store: RecordStore,
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
    #[instrument(level = "debug", name = "server_create", skip_all)]
    pub async fn new(config_path: &Path) -> Result<Self, Error> {
        let config = Config::from_file(config_path)?;

        let record_store = RecordStore::new();
        let server_state = ServerState::new(record_store.receiver(), config.zones.clone());

        let http_client = Client::builder()
            .dns_resolver(Arc::new(server_state.clone()))
            .build()?;

        let sources = Sources::new(record_store.clone(), http_client);

        let server = Self {
            config: Arc::new(Mutex::new(config.clone())),
            record_store: record_store.clone(),
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
            .and_then(|api_config| ApiServer::new(api_config, record_store))
        {
            server.api_server.replace(api_server).await;
        }

        {
            let mut sources = server.sources.lock().await;
            sources.install_sources(config, None).await;
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
    }

    #[instrument(level = "debug", name = "server_shutdown", skip(self))]
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

    #[instrument(level = "debug", name = "update_config" skip_all)]
    async fn update_config(&self, new_config: Config) {
        let (restart_server, restart_api_server, old_config) = {
            let mut config = self.config.lock().await;

            let restart_server = config.server != new_config.server;
            let restart_api_server = config.api != new_config.api;

            let mut old_config = new_config.clone();
            mem::swap(config.deref_mut(), &mut old_config);
            self.server_state.replace_zones(config.zones.clone()).await;

            (restart_server, restart_api_server, old_config)
        };

        {
            let mut sources = self.sources.lock().await;
            sources
                .install_sources(new_config.clone(), Some(&old_config))
                .await;
        }

        if restart_server {
            let mut dns_server = self.dns_server.lock().await;
            dns_server.restart(&new_config.server).await;
        }

        if restart_api_server {
            if let Some(old_server) = self.api_server.take().await {
                old_server.shutdown().await;
            }

            if let Some(api_server) = new_config
                .api
                .as_ref()
                .and_then(|api_config| ApiServer::new(api_config, self.record_store.clone()))
            {
                self.api_server.replace(api_server).await;
            }
        }
    }
}
