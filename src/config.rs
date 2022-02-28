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
    #[serde(skip)]
    config_file: PathBuf,

    #[serde(default)]
    pub sources: SourceConfig,
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Config, String> {
        let f = File::open(path)
            .map_err(|e| format!("Failed to open file at {}: {}", path.display(), e))?;

        let mut config: Config = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        config.config_file = path.to_owned();
        Ok(config)
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.join(path).canonicalize().unwrap()
    }
}

pub fn config_stream(config_file: &Path) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE.clone());
    let file = config_file.to_owned();

    tokio::spawn(async move {
        let mut config = Config::from_file(&file);

        loop {
            let actual_config = match config {
                Ok(ref config) => config.clone(),
                Err(ref e) => {
                    log::error!("{}", e);
                    let mut config = Config::default();
                    config.config_file = file.to_owned();
                    config
                }
            };

            if let Err(e) = sender.send(actual_config).await {
                log::error!("Failed to send updated config: {}", e);
                return;
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
