use futures::StreamExt;
use std::{
    collections::HashMap,
    env, fmt,
    fs::File,
    path::{Path, PathBuf},
};
use tokio::sync::watch;
use trust_dns_server::{client::rr::rdata::SOA, proto::rr};

use crate::{
    api::ApiConfig, dns::Fqdn, dns::ServerConfig, dns::Upstream, sources::SourceConfig,
    watcher::watch,
};

mod file;

pub use file::deserialize_url;

pub struct ZoneConfig {
    origin: Option<Fqdn>,
    pub upstream: Option<Upstream>,
    pub ttl: u32,
    authoritative: bool,
}

impl ZoneConfig {
    fn new(defaults: &file::ZoneConfig) -> Self {
        Self {
            origin: None,
            upstream: defaults.upstream.clone(),
            ttl: defaults.ttl.unwrap_or(300),
            authoritative: false,
        }
    }

    pub fn soa(&self) -> Option<rr::Record> {
        if !self.authoritative {
            return None;
        }

        let origin = self.origin.clone()?;

        Some(rr::Record::from_rdata(
            origin.name()?.clone(),
            self.ttl,
            rr::RData::SOA(SOA::new(
                origin.child("ns").name()?.clone(),
                origin.child("hostmaster").name()?.clone(),
                0,
                self.ttl.try_into().unwrap(),
                self.ttl.try_into().unwrap(),
                (self.ttl * 10).try_into().unwrap(),
                60,
            )),
        ))
    }

    fn apply_config(&mut self, origin: Fqdn, config: &file::ZoneConfig) {
        self.origin = Some(origin);

        if let Some(ref upstream) = config.upstream {
            self.upstream = Some(upstream.clone());
        }
        if let Some(ttl) = config.ttl {
            self.ttl = ttl;
        }
        self.authoritative = config.authoritative.unwrap_or(true);
    }
}

impl fmt::Debug for ZoneConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if let Some(ref origin) = self.origin {
            parts.push(format!("origin={}", origin));
        }

        parts.push(format!("ttl={}", self.ttl));
        parts.push(format!("authoritative={}", self.authoritative));

        if let Some(ref upstream) = self.upstream {
            parts.push(format!("upstream={:?}", upstream));
        }

        f.pad(&format!("[{}]", parts.join(" ")))
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Zones {
    pub defaults: file::ZoneConfig,
    pub zones: Vec<(Fqdn, file::ZoneConfig)>,
}

impl Zones {
    fn new(defaults: file::ZoneConfig, mut zones: HashMap<Fqdn, file::ZoneConfig>) -> Self {
        let mut zones: Vec<(Fqdn, file::ZoneConfig)> = zones.drain().collect();
        zones.sort_by(|(n1, _), (n2, _)| n1.cmp(n2));

        Self { defaults, zones }
    }

    fn build_config(&self, name: &Fqdn) -> ZoneConfig {
        let mut config = ZoneConfig::new(&self.defaults);

        for (n, c) in &self.zones {
            if n.is_parent(name) {
                config.apply_config(n.clone(), c);
            }
        }

        config
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Config {
    pub config_file: PathBuf,
    pub server: ServerConfig,
    pub api: Option<ApiConfig>,
    pub sources: SourceConfig,
    zones: Zones,
}

impl Config {
    pub fn from_file(config_file: &Path) -> Result<Config, String> {
        let f = File::open(config_file)
            .map_err(|e| format!("Failed to open file at {}: {}", config_file.display(), e))?;

        let config: file::ConfigFile = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        Ok(Config {
            config_file: config_file.to_owned(),
            server: config.server,
            api: config.api,
            sources: config.sources,
            zones: Zones::new(config.defaults, config.zones),
        })
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.parent().unwrap().join(path)
    }

    pub fn default(config_file: &Path) -> Self {
        Config {
            config_file: config_file.to_owned(),
            ..Default::default()
        }
    }

    pub fn zone_config(&self, name: &Fqdn) -> ZoneConfig {
        self.zones.build_config(name)
    }
}

pub fn config_stream(args: &[String]) -> watch::Receiver<Config> {
    let config_file = config_file(args.get(1));
    log::info!("Reading configuration from {}.", config_file.display());
    let mut file_stream = watch(&config_file).unwrap();

    let mut config = Config::from_file(&config_file);

    let (sender, receiver) = watch::channel(match config {
        Ok(ref c) => c.clone(),
        Err(ref e) => {
            log::error!("{}", e);
            Config::default(&config_file)
        }
    });

    tokio::spawn(async move {
        loop {
            file_stream.next().await;

            let next_config = Config::from_file(&config_file);
            if next_config == config {
                continue;
            }

            match next_config {
                Ok(ref actual_config) => {
                    if let Err(e) = sender.send(actual_config.clone()) {
                        log::error!("Failed to send updated config: {}", e);
                        return;
                    }

                    if config.is_err() {
                        log::info!("Successfully read new config");
                    }
                }
                Err(ref e) => {
                    if config.as_ref() != Err(e) {
                        log::error!("{}", e);
                    }
                }
            }

            config = next_config;
        }
    });

    receiver
}

fn config_file(arg: Option<&String>) -> PathBuf {
    if let Some(str) = arg {
        PathBuf::from(str).canonicalize().unwrap()
    } else if let Ok(value) = env::var("LOCALNS_CONFIG") {
        PathBuf::from(value).canonicalize().unwrap()
    } else {
        PathBuf::from("config.yaml").canonicalize().unwrap()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn zone_config() {}
}
