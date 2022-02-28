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
    pub fn from_file(path: &Path) -> Result<Config, String> {
        let f =
            File::open(path).map_err(|e| format!("Failed to open configuration file: {}", e))?;

        serde_yaml::from_reader(f).map_err(|e| format!("Failed to parse configuration: {}", e))
    }
}

pub fn config_stream(config_file: &Path) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE.clone());
    let file = config_file.to_owned();

    tokio::spawn(async move {
        let mut config = Config::from_file(&file);

        loop {
            match config {
                Ok(ref config) => {
                    if let Err(e) = sender.send(config.clone()).await {
                        log::error!("Failed to send updated config: {}", e);
                        return;
                    }
                }
                Err(ref e) => log::error!("{}", e),
            }

            loop {
                sleep(Duration::from_millis(500)).await;

                let next_config = Config::from_file(&file);
                if next_config != config {
                    config = next_config;
                    break;
                }
            }
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
