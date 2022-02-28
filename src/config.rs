use serde::Deserialize;
use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;

use crate::{debounce::Debounced, sources::SourceConfig};

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub sources: SourceConfig,
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

pub fn config_stream(config_file: &Path) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE.clone());
    let file = config_file.to_owned();

    tokio::spawn(async move {
        loop {
            let config = Config::from_file(&file);

            if let Err(e) = sender.send(config).await {
                log::error!("Failed to send updated config: {}", e);
                return;
            }

            sleep(Duration::from_millis(500)).await;
        }
    });

    stream
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
