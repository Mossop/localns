use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("DNS protocol error: {source}")]
    DnsError {
        #[from]
        source: hickory_server::proto::error::ProtoError,
    },
    #[error("IO error: {source}")]
    IoError {
        #[from]
        source: io::Error,
    },
    #[error("Config file error: {source}")]
    ConfigParseError {
        #[from]
        source: figment::Error,
    },
    #[error("Failed to watch config file: {source}")]
    WatchError {
        #[from]
        source: notify::Error,
    },
    #[error("Yaml parse error: {source}")]
    YamlParseError {
        #[from]
        source: serde_yaml::Error,
    },
    #[error("Docker daemon error: {source}")]
    DockerError {
        #[from]
        source: bollard::errors::Error,
    },
    #[error("Docker API error: {message}")]
    DockerApiError { message: String },
    #[error("File {file} is an invalid type")]
    FileTypeError { file: PathBuf },
    #[error("Error parsing traefik rule '{rule}': {message}")]
    TraefikRuleError { rule: String, message: String },
}
