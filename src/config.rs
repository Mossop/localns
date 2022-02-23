use futures::Stream;
use notify::{watcher, RecommendedWatcher, RecursiveMode, Watcher};
use pin_project_lite::pin_project;
use serde::Deserialize;
use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc::channel,
    task::Context,
    thread,
    time::Duration,
};
use tokio::sync::watch::{self, Receiver};
use tokio_stream::wrappers::WatchStream;

use crate::debounce::Debounced;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DockerConfig {
    pub file: String,
    pub address: String,
    pub private_key: Option<PathBuf>,
    pub certificate: Option<PathBuf>,
    pub ca: Option<PathBuf>,
}

#[derive(Clone, Deserialize, PartialEq, Eq, Debug)]
pub struct Ttl(u64);
impl Default for Ttl {
    fn default() -> Self {
        Ttl(300)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct Config {
    pub zone: String,
    #[serde(default)]
    pub ttl: Ttl,
    pub docker: Option<DockerConfig>,
}

impl Config {
    pub fn from_file(path: &Path) -> Option<Config> {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                log::error!("Failed to open configuration file: {}", e);
                return None;
            }
        };

        match serde_yaml::from_reader(f) {
            Ok(config) => {
                log::trace!("Parsed configuration: {:?}", config);
                Some(config)
            }
            Err(e) => {
                log::error!("Failed to parse configuration: {}", e);
                None
            }
        }
    }
}

pin_project! {
    pub struct ConfigStream {
        receiver: Receiver<Option<Config>>,
        #[pin]
        inner: WatchStream<Option<Config>>,
        file_watcher: Option<RecommendedWatcher>,
    }
}

impl Clone for ConfigStream {
    fn clone(&self) -> Self {
        ConfigStream {
            receiver: self.receiver.clone(),
            inner: WatchStream::new(self.receiver.clone()),
            file_watcher: None,
        }
    }
}

impl ConfigStream {
    pub fn new(config_file: &Path) -> Debounced<Self> {
        let config = Config::from_file(config_file);
        let (sender, receiver) = watch::channel(config);

        let (watcher_sender, watcher_receiver) = channel();
        let file_watcher = match watcher(watcher_sender, Duration::from_millis(500)) {
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

        Debounced::new(
            ConfigStream {
                inner: WatchStream::new(receiver.clone()),
                receiver,
                file_watcher,
            },
            Duration::from_millis(500),
        )
    }
}

impl Stream for ConfigStream {
    type Item = Option<Config>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        this.inner.poll_next(cx)
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
