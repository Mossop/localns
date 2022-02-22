use serde::Deserialize;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize, Clone)]
pub struct DockerConfig {
    pub file: String,
    pub address: String,
    pub private_key: Option<PathBuf>,
    pub certificate: Option<PathBuf>,
    pub ca: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
pub struct Ttl(u64);
impl Default for Ttl {
    fn default() -> Self {
        Ttl(300)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub zone: String,
    #[serde(default)]
    pub ttl: Ttl,
    pub docker: Option<DockerConfig>,
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Config, String> {
        let f =
            File::open(path).map_err(|e| format!("Failed to open configuration file: {}", e))?;
        serde_yaml::from_reader(f).map_err(|e| format!("Failed to parse configuration: {}", e))
    }

    pub fn from_str(str: &str) -> Result<Config, String> {
        serde_yaml::from_str(str).map_err(|e| format!("Failed to parse configuration: {}", e))
    }
}
