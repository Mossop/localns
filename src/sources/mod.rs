use std::{
    collections::HashMap,
    future::{pending, Pending},
    sync::Arc,
};

use futures::future::{abortable, AbortHandle, AbortRegistration, Abortable};
use serde::Deserialize;
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use crate::{config::Config, record::RecordSet};

pub mod dhcp;
pub mod docker;
pub mod file;
pub mod traefik;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub docker: HashMap<String, docker::DockerConfig>,

    #[serde(default)]
    pub traefik: HashMap<String, traefik::TraefikConfig>,

    #[serde(default)]
    pub dhcp: HashMap<String, dhcp::DhcpConfig>,

    #[serde(default)]
    pub file: HashMap<String, file::FileConfig>,
}

#[derive(Clone, Hash)]
struct SourceKey;

struct RecordSourcesState {
    sources: HashMap<Uuid, RecordSource>,
    sender: watch::Sender<RecordSet>,
}

impl RecordSourcesState {
    fn new(sender: watch::Sender<RecordSet>) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            sources: HashMap::new(),
            sender,
        }))
    }

    fn new_source(&mut self) -> Uuid {
        let uuid = Uuid::new_v4();
        self.sources.insert(uuid, Default::default());
        uuid
    }

    fn set_records(&mut self, source: &Uuid, records: RecordSet) {
        if let Some(source) = self.sources.get_mut(source) {
            if source.records == records {
                return;
            }

            source.records = records;
        }

        let all_records = self
            .sources
            .values()
            .flat_map(|source| source.records.clone())
            .collect();

        if let Err(e) = self.sender.send(all_records) {
            log::trace!("Failed to send new records: {}", e);
        }
    }

    fn register_abort(&mut self, source: &Uuid, handle: AbortHandle) {
        let source = self.sources.get_mut(source).unwrap();
        source.aborters.push(handle);
    }

    fn abort(&mut self) {
        for (_, source) in self.sources.drain() {
            for handle in source.aborters {
                handle.abort();
            }
        }
    }
}

#[derive(Default)]
struct RecordSource {
    records: RecordSet,
    aborters: Vec<AbortHandle>,
}

struct SourceContext {
    uuid: Uuid,
    state: Arc<Mutex<RecordSourcesState>>,
    record_waiter: Option<AbortHandle>,
}

impl SourceContext {
    fn send(&mut self, records: RecordSet) {
        let state = self.state.clone();
        let uuid = self.uuid;
        let waiter = self.record_waiter.take();

        tokio::spawn(async move {
            let mut sources = state.lock().await;
            sources.set_records(&uuid, records);

            if let Some(handle) = waiter {
                handle.abort();
            }
        });
    }

    fn abort_registration(&self) -> AbortRegistration {
        let state = self.state.clone();
        let uuid = self.uuid;
        let (handle, registration) = AbortHandle::new_pair();

        tokio::spawn(async move {
            let mut sources = state.lock().await;
            sources.register_abort(&uuid, handle)
        });

        registration
    }
}

pub struct RecordSources {
    state: Arc<Mutex<RecordSourcesState>>,
    receiver: watch::Receiver<RecordSet>,
}

impl RecordSources {
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(RecordSet::new());
        let state = RecordSourcesState::new(sender);

        Self { state, receiver }
    }

    pub async fn replace_sources(&mut self, config: &Config) {
        self.drop_sources().await;

        // DHCP is assumed to not need any additional resolution.
        for (name, dhcp_config) in config.dhcp_sources() {
            log::trace!("Adding dhcp source {}", name);
            let (context, pending) = self.add_source().await;
            dhcp::source(name.clone(), config.clone(), dhcp_config.clone(), context);
            assert!(pending.await.is_err());
        }

        // File sources are assumed to not need any additional resolution.
        for (name, file_config) in config.file_sources() {
            log::trace!("Adding file source {}", name);
            let (context, pending) = self.add_source().await;
            file::source(name.clone(), config.clone(), file_config.clone(), context);
            assert!(pending.await.is_err());
        }

        // Docker hostname may depend on DHCP records above.
        for (name, docker_config) in config.docker_sources() {
            log::trace!("Adding docker source {}", name);
            let (context, pending) = self.add_source().await;
            docker::source(name.clone(), config.clone(), docker_config.clone(), context);
            assert!(pending.await.is_err());
        }

        // Traefik hostname my depend on Docker or DHCP records.
        for (name, traefik_config) in config.traefik_sources() {
            log::trace!("Adding traefik source {}", name);
            let (context, pending) = self.add_source().await;
            traefik::source(name.clone(), traefik_config.clone(), context);
            assert!(pending.await.is_err());
        }
    }

    async fn add_source(&mut self) -> (SourceContext, Abortable<Pending<()>>) {
        let (future, handle) = abortable(pending());
        let mut state = self.state.lock().await;

        (
            SourceContext {
                uuid: state.new_source(),
                state: self.state.clone(),
                record_waiter: Some(handle),
            },
            future,
        )
    }

    pub fn receiver(&mut self) -> watch::Receiver<RecordSet> {
        self.receiver.clone()
    }

    async fn drop_sources(&self) {
        let mut state = self.state.lock().await;
        state.abort();
    }

    pub async fn destroy(self) {
        self.drop_sources().await;
    }
}

impl Default for RecordSources {
    fn default() -> Self {
        Self::new()
    }
}
