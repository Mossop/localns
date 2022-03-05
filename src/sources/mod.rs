use std::{
    collections::HashMap,
    collections::HashSet,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::{AbortHandle, AbortRegistration},
    Stream, StreamExt,
};
use pin_project_lite::pin_project;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{config::Config, debounce::Debounced, rfc1035::RecordSet};

pub mod docker;
pub mod traefik;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub docker: HashMap<String, docker::DockerConfig>,

    #[serde(default)]
    pub traefik: HashMap<String, traefik::TraefikConfig>,
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
        is_first: bool,
    }
}

impl RecordSources {
    pub async fn from_config(config: &Config) -> Self {
        let mut sources = Self {
            sources: Vec::new(),
            is_first: true,
        };

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
                    config.clone(),
                    traefik_config.clone(),
                ))
                .await;
        }

        sources
    }

    pub async fn add_source(&mut self, mut source: RecordSource) {
        match source.stream.next().await {
            Some(records) => self.sources.push(Item {
                source,
                finished: false,
                current: records,
            }),
            None => self.sources.push(Item {
                source,
                finished: true,
                current: HashSet::new(),
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
        let mut new_set = HashSet::new();
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
            Poll::Ready(Some(new_set))
        } else {
            Poll::Pending
        }
    }
}
