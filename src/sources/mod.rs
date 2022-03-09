use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::{AbortHandle, AbortRegistration},
    stream::{Stream, StreamExt},
};
use pin_project_lite::pin_project;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    config::Config,
    debounce::Debounced,
    record::{RecordSet, SharedRecordSet},
};

pub mod dhcp;
pub mod docker;
pub mod traefik;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub docker: HashMap<String, docker::DockerConfig>,

    #[serde(default)]
    pub traefik: HashMap<String, traefik::TraefikConfig>,

    #[serde(default)]
    pub dhcp: HashMap<String, dhcp::DhcpConfig>,
}

pub struct RecordSource {
    abort: AbortHandle,
    stream: Debounced<ReceiverStream<RecordSet>>,
}

fn create_source() -> (mpsc::Sender<RecordSet>, AbortRegistration, RecordSource) {
    let (sender, receiver) = mpsc::channel(5);
    let (handle, registration) = AbortHandle::new_pair();
    let stream = Debounced::new(ReceiverStream::new(receiver), Duration::from_millis(500));

    (
        sender,
        registration,
        RecordSource {
            abort: handle,
            stream,
        },
    )
}

struct Item {
    source: RecordSource,
    finished: bool,
    current: RecordSet,
}

pin_project! {
    pub struct RecordSources {
        sources: Vec<Item>,
        records: SharedRecordSet,
        is_first: bool,
    }
}

impl RecordSources {
    pub async fn from_config(config: &Config) -> Self {
        let mut sources = Self {
            sources: Vec::new(),
            records: SharedRecordSet::new(RecordSet::new()),
            is_first: true,
        };

        for (name, dhcp_config) in config.dhcp_sources() {
            log::trace!("Adding dhcp source {}", name);
            sources
                .add_source(dhcp::source(
                    name.clone(),
                    config.clone(),
                    dhcp_config.clone(),
                ))
                .await;
        }

        for (name, docker_config) in config.docker_sources() {
            log::trace!("Adding docker source {}", name);
            sources
                .add_source(docker::source(
                    name.clone(),
                    config.clone(),
                    docker_config.clone(),
                ))
                .await;
        }

        for (name, traefik_config) in config.traefik_sources() {
            log::trace!("Adding traefik source {}", name);
            sources
                .add_source(traefik::source(
                    name.clone(),
                    traefik_config.clone(),
                    sources.records.clone(),
                ))
                .await;
        }

        sources
    }

    pub async fn add_source(&mut self, mut source: RecordSource) {
        match source.stream.next().await {
            Some(records) => {
                self.sources.push(Item {
                    source,
                    finished: false,
                    current: records.clone(),
                });
                self.records.add_records(records)
            }
            None => self.sources.push(Item {
                source,
                finished: true,
                current: RecordSet::new(),
            }),
        }
    }

    pub fn destroy(self) {
        for source in self.sources {
            source.source.abort.abort()
        }
    }
}

impl Stream for RecordSources {
    type Item = RecordSet;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let mut is_new = *this.is_first;
        let mut new_set = RecordSet::new();
        *this.is_first = false;

        for source in this.sources.iter_mut() {
            if !source.finished {
                match source.source.stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(set)) => {
                        is_new = true;
                        source.current = set.clone();

                        new_set.reserve(set.len());
                        for record in set {
                            new_set.insert(record);
                        }
                    }
                    Poll::Ready(None) => {
                        source.finished = true;
                        is_new = true;
                    }
                    Poll::Pending => {
                        new_set.reserve(source.current.len());
                        for record in source.current.iter() {
                            new_set.insert(record.clone());
                        }
                    }
                }
            }
        }

        if is_new {
            this.records.replace_records(new_set.clone());
            Poll::Ready(Some(new_set))
        } else {
            Poll::Pending
        }
    }
}
