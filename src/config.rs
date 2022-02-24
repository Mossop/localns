use futures::{ready, Stream};
use notify::{watcher, RecommendedWatcher, RecursiveMode, Watcher};
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc::channel,
    task::{Context, Poll},
    thread,
    time::Duration,
};
use tokio::sync::watch::{self, Receiver};
use tokio_stream::wrappers::WatchStream;

use crate::debounce::Debounced;
use crate::docker::DockerConfig;

const FILE_DEBOUNCE: Duration = Duration::from_millis(500);
const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Deserialize, PartialEq, Eq, Debug)]
pub struct Ttl(u64);
impl Default for Ttl {
    fn default() -> Self {
        Ttl(300)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ttl: Ttl,
    pub docker: Option<DockerConfig>,
}

impl Config {
    pub fn from_file(path: &Path) -> Config {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                log::error!("Failed to open configuration file: {}", e);
                return Default::default();
            }
        };

        match serde_yaml::from_reader(f) {
            Ok(config) => config,
            Err(e) => {
                log::error!("Failed to parse configuration: {}", e);
                Default::default()
            }
        }
    }
}

pin_project! {
    pub struct ConfigStream {
        pub config: Config,
        receiver: Receiver<Config>,
        #[pin]
        inner: Debounced<WatchStream<Config>>,
        file_watcher: Option<RecommendedWatcher>,
    }
}

impl Clone for ConfigStream {
    fn clone(&self) -> Self {
        ConfigStream {
            config: self.config.clone(),
            receiver: self.receiver.clone(),
            inner: Debounced::new(
                WatchStream::new(self.receiver.clone()),
                CONFIG_DEBOUNCE.clone(),
            ),
            file_watcher: None,
        }
    }
}

impl ConfigStream {
    pub fn new(config_file: &Path) -> Self {
        let config = Config::from_file(config_file);
        let (sender, receiver) = watch::channel(config.clone());

        let (watcher_sender, watcher_receiver) = channel();
        let file_watcher = match watcher(watcher_sender, FILE_DEBOUNCE.clone()) {
            Ok(mut w) => {
                if let Err(e) = w.watch(config_file, RecursiveMode::NonRecursive) {
                    log::error!("Failed to create config file watcher: {}", e);
                }
                Some(w)
            }
            Err(e) => {
                log::error!("Failed to create config file watcher: {}", e);
                None
            }
        };

        let file = config_file.to_owned();
        thread::spawn(move || {
            log::trace!("Starting configuration watcher loop.");
            loop {
                match watcher_receiver.recv() {
                    Ok(_) => {
                        log::trace!("Saw configuration file change.");

                        if let Err(e) = sender.send(Config::from_file(&file)) {
                            log::error!("Failed to send new configuration: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        log::trace!("File watcher error: {}", e);
                        break;
                    }
                }
            }
            log::trace!("Exiting configuration watcher loop.");
        });

        ConfigStream {
            config,
            inner: Debounced::new(WatchStream::new(receiver.clone()), CONFIG_DEBOUNCE.clone()),
            receiver,
            file_watcher,
        }
    }
}

impl Stream for ConfigStream {
    type Item = Config;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let poll_result = ready!(this.inner.poll_next(cx));

        if let Some(config) = poll_result {
            *this.config = config.clone();
            Poll::Ready(Some(config))
        } else {
            Poll::Ready(None)
        }
    }
}

pub fn config_file(arg: Option<String>) -> PathBuf {
    if let Some(str) = arg {
        PathBuf::from(str)
    } else if let Ok(value) = env::var("DOCKER_DNS_CONFIG") {
        PathBuf::from(value)
    } else {
        PathBuf::from("config.yml")
    }
}
