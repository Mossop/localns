use serde::Deserialize;
use std::{
    collections::hash_map::Iter,
    env,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    debounce::Debounced,
    sources::{docker::DockerConfig, SourceConfig},
};

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    pub sources: SourceConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub config_file: PathBuf,
    pub target_dir: PathBuf,
    config: ConfigFile,
}

impl Config {
    pub fn from_file(config_file: &Path, target_dir: &Path) -> Result<Config, String> {
        let f = File::open(config_file)
            .map_err(|e| format!("Failed to open file at {}: {}", config_file.display(), e))?;

        let config: ConfigFile = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        Ok(Config {
            config_file: config_file.to_owned(),
            target_dir: target_dir.to_owned(),
            config,
        })
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.join(path).canonicalize().unwrap()
    }

    pub fn default(config_file: &Path, target_dir: &Path) -> Self {
        Config {
            config_file: config_file.to_owned(),
            target_dir: target_dir.to_owned(),
            config: ConfigFile::default(),
        }
    }

    pub fn docker_sources(&self) -> Iter<String, DockerConfig> {
        self.config.sources.docker.iter()
    }
}

pub fn config_stream(args: &Vec<String>) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE.clone());
    let config_file = config_file(args.get(1));
    let target_dir = target_dir();

    log::info!("Reading configuration from {}", config_file.display());

    tokio::spawn(async move {
        let mut config = Config::from_file(&config_file, &target_dir);

        loop {
            let actual_config = match config {
                Ok(ref config) => config.clone(),
                Err(ref e) => {
                    log::error!("{}", e);
                    Config::default(&config_file, &target_dir)
                }
            };

            if let Err(e) = sender.send(actual_config).await {
                log::error!("Failed to send updated config: {}", e);
                return;
            }

            loop {
                sleep(Duration::from_millis(500)).await;

                let next_config = Config::from_file(&config_file, &target_dir);
                if next_config != config {
                    config = next_config;
                    break;
                }
            }
        }
    });

    stream
}

fn config_file(arg: Option<&String>) -> PathBuf {
    if let Some(str) = arg {
        PathBuf::from(str)
    } else if let Ok(value) = env::var("DOCKER_DNS_CONFIG") {
        PathBuf::from(value)
    } else {
        PathBuf::from("config.yml")
    }
}

fn target_dir() -> PathBuf {
    if let Ok(value) = env::var("DOCKER_DNS_ZONE_DIR") {
        PathBuf::from(value)
    } else {
        env::current_dir().unwrap()
    }
}
