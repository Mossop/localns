use local_ip_address::local_ip;
use serde::Deserialize;
use std::{
    collections::hash_map::Iter,
    collections::HashMap,
    env,
    fmt::Display,
    fs::File,
    io,
    io::Write,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    debounce::Debounced,
    rfc1035::{AbsoluteName, Address, Class, RecordSet, Zone},
    sources::{docker::DockerConfig, traefik::TraefikConfig, SourceConfig},
    RecordData, ResourceRecord,
};

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub enum UpstreamProtocol {
    Tls,
    Dns,
}

impl Display for UpstreamProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamProtocol::Dns => f.pad("dns"),
            UpstreamProtocol::Tls => f.pad("tls"),
        }
    }
}

impl Default for UpstreamProtocol {
    fn default() -> Self {
        UpstreamProtocol::Dns
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub protocol: UpstreamProtocol,
    pub address: IpAddr,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Upstream {
    IpAddr(IpAddr),
    Config(UpstreamConfig),
}

impl Upstream {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        match self {
            Upstream::IpAddr(ip) => {
                writeln!(buf, "  forward . {}", ip)
            }
            Upstream::Config(config) => {
                writeln!(
                    buf,
                    "  forward . {}://{} {{",
                    config.protocol, config.address
                )?;
                writeln!(buf, "  }}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct ZoneConfig {
    #[serde(default)]
    upstream: Option<Upstream>,

    #[serde(default)]
    authoratative: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
struct ConfigFile {
    ip_address: Option<IpAddr>,

    #[serde(default)]
    pub upstream: Option<Upstream>,

    #[serde(default)]
    pub sources: SourceConfig,

    #[serde(default)]
    pub zones: HashMap<String, ZoneConfig>,
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
        self.config_file.parent().unwrap().join(path)
    }

    pub fn zone_path(&self, path: &str) -> PathBuf {
        self.target_dir.join(path)
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

    pub fn traefik_sources(&self) -> Iter<String, TraefikConfig> {
        self.config.sources.traefik.iter()
    }

    pub fn ip_address(&self) -> Result<IpAddr, String> {
        if let Some(ip) = self.config.ip_address {
            Ok(ip)
        } else {
            local_ip().map_err(|e| {
                format!(
                    "Unable to find local IP address, please specify in config: {}",
                    e
                )
            })
        }
    }

    pub fn upstream(&self) -> &Option<Upstream> {
        &self.config.upstream
    }

    fn zone(&self, domain: AbsoluteName, local_ip: &IpAddr) -> Zone {
        let config = self.config.zones.get(&domain.non_absolute());

        Zone::new(domain, local_ip, config.and_then(|c| c.upstream.clone()))
    }

    pub fn zones(&self, records: RecordSet) -> Vec<Zone> {
        let local_ip = match self.ip_address() {
            Ok(ip) => ip,
            Err(e) => {
                log::error!("{}", e);
                return Vec::new();
            }
        };

        let mut zones: HashMap<AbsoluteName, Zone> = Default::default();

        for domain in self.config.zones.keys() {
            let zone = self.zone(domain.into(), &local_ip);
            zones.insert(zone.domain.clone(), zone);
        }

        for record in records {
            if let Some((name, domain)) = record.name.split() {
                let resource = ResourceRecord {
                    name: Some(name.clone()),
                    class: Class::In,
                    ttl: 300,
                    data: record.data.clone(),
                };

                match zones.get_mut(&domain) {
                    Some(zone) => zone.add_record(resource),
                    None => {
                        let mut zone = self.zone(domain, &local_ip);
                        zone.add_record(resource);
                        zones.insert(zone.domain.clone(), zone);
                    }
                };
            }
        }

        let domains: Vec<AbsoluteName> = zones.keys().cloned().collect();

        for domain in domains {
            if let Some((host, parent_domain)) = domain.split() {
                if let Some(parent) = zones.get_mut(&parent_domain) {
                    parent.add_record(ResourceRecord {
                        name: Some(host),
                        class: Class::In,
                        ttl: 300,
                        data: RecordData::Ns(Address::from(local_ip)),
                    })
                }
            }
        }

        zones.into_values().collect()
    }
}

pub fn config_stream(args: &[String]) -> Debounced<ReceiverStream<Config>> {
    let (sender, receiver) = mpsc::channel(5);
    let stream = Debounced::new(ReceiverStream::new(receiver), CONFIG_DEBOUNCE);
    let config_file = config_file(args.get(1));
    let target_dir = target_dir();

    log::info!(
        "Reading configuration from {}. Writing zones to {}",
        config_file.display(),
        target_dir.display()
    );

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
        PathBuf::from(str).canonicalize().unwrap()
    } else if let Ok(value) = env::var("LOCALNS_CONFIG") {
        PathBuf::from(value).canonicalize().unwrap()
    } else {
        PathBuf::from("config.yaml").canonicalize().unwrap()
    }
}

fn target_dir() -> PathBuf {
    if let Ok(value) = env::var("LOCALNS_ZONE_DIR") {
        PathBuf::from(value).canonicalize().unwrap()
    } else {
        env::current_dir().unwrap().canonicalize().unwrap()
    }
}
