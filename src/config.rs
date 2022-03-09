use futures::StreamExt;
use reqwest::Url;
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{
    collections::hash_map::Iter,
    collections::HashMap,
    env,
    fmt::{self, Display},
    fs::File,
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio::sync::watch;

use crate::{
    record::{fqdn, Name, RData, RecordSet},
    server::{ServerConfig, Zone},
    sources::{
        dhcp::DhcpConfig, docker::DockerConfig, file::FileConfig, traefik::TraefikConfig,
        SourceConfig,
    },
    upstream::{Upstream, UpstreamConfig},
    watcher::watch,
};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Hash)]
#[serde(from = "String")]
pub enum Host {
    Name(String),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

impl Host {
    pub fn rdata(&self) -> RData {
        match self {
            Host::Name(name) => RData::CNAME(fqdn(name)),
            Host::Ipv4(ip) => RData::A(*ip),
            Host::Ipv6(ip) => RData::AAAA(*ip),
        }
    }
}

impl Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Name(st) => f.pad(st),
            Host::Ipv4(ip) => f.pad(&ip.to_string()),
            Host::Ipv6(ip) => f.pad(&ip.to_string()),
        }
    }
}

impl From<&str> for Host {
    fn from(host: &str) -> Self {
        if let Ok(ip) = host.parse() {
            Host::Ipv4(ip)
        } else if let Ok(ip) = host.parse() {
            Host::Ipv6(ip)
        } else {
            Host::Name(host.into())
        }
    }
}

impl From<String> for Host {
    fn from(host: String) -> Self {
        if let Ok(ip) = host.parse() {
            Host::Ipv4(ip)
        } else if let Ok(ip) = host.parse() {
            Host::Ipv6(ip)
        } else {
            Host::Name(host)
        }
    }
}

impl From<Ipv4Addr> for Host {
    fn from(ip: Ipv4Addr) -> Self {
        Host::Ipv4(ip)
    }
}

impl From<Ipv6Addr> for Host {
    fn from(ip: Ipv6Addr) -> Self {
        Host::Ipv6(ip)
    }
}

impl From<IpAddr> for Host {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => ip.into(),
            IpAddr::V6(ip) => ip.into(),
        }
    }
}

impl From<&Name> for Host {
    fn from(name: &Name) -> Self {
        let mut clone = name.clone();
        if !clone.is_fqdn() {
            panic!("Expected a FQDN");
        }

        clone.set_fqdn(false);
        Host::Name(clone.to_ascii())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Hash)]
#[serde(from = "String")]
pub struct Address {
    pub host: Host,
    pub port: Option<u16>,
}

impl Address {
    pub fn address(&self, default_port: u16) -> String {
        format!("{}:{}", self.host, self.port.unwrap_or(default_port))
    }

    pub fn to_socket_address(&self, default_port: u16) -> Result<SocketAddr, AddrParseError> {
        SocketAddr::from_str(&format!(
            "{}:{}",
            self.host,
            self.port.unwrap_or(default_port)
        ))
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(port) = self.port {
            f.pad(&format!("{}:{}", self.host, port))
        } else {
            f.pad(&self.host.to_string())
        }
    }
}

impl From<&str> for Address {
    fn from(host: &str) -> Self {
        if let Ok(addr) = SocketAddr::from_str(host) {
            Self {
                host: addr.ip().into(),
                port: Some(addr.port()),
            }
        } else if let Ok(ip) = host.parse::<Ipv4Addr>() {
            Host::from(ip).into()
        } else if let Ok(ip) = host.parse::<Ipv6Addr>() {
            Host::from(ip).into()
        } else if let Some(pos) = host.rfind(':') {
            if let Ok(port) = host[pos + 1..].parse::<u16>() {
                Self {
                    host: host[0..pos].into(),
                    port: Some(port),
                }
            } else {
                Self {
                    host: host.into(),
                    port: None,
                }
            }
        } else {
            Self {
                host: host.into(),
                port: None,
            }
        }
    }
}

impl From<String> for Address {
    fn from(host: String) -> Self {
        Address::from(host.as_str())
    }
}

impl From<Host> for Address {
    fn from(host: Host) -> Self {
        Self { host, port: None }
    }
}

struct NameVisitor;

impl<'de> Visitor<'de> for NameVisitor {
    type Value = Name;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Name::parse(value, None)
            .map(|mut n| {
                n.set_fqdn(true);
                n
            })
            .map_err(|e| E::custom(format!("{}", e)))
    }
}

pub fn deserialize_fqdn<'de, D>(de: D) -> Result<Name, D::Error>
where
    D: Deserializer<'de>,
{
    de.deserialize_str(NameVisitor)
}

struct UrlVisitor;

impl<'de> Visitor<'de> for UrlVisitor {
    type Value = Url;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string that parses as a URL")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Url::parse(value).map_err(|e| E::custom(format!("{}", e)))
    }
}

pub fn deserialize_url<'de, D>(de: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    de.deserialize_str(UrlVisitor)
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct ZoneConfig {
    #[serde(default)]
    upstream: Option<UpstreamConfig>,

    #[serde(default)]
    authoratative: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    pub upstream: Option<UpstreamConfig>,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub sources: SourceConfig,

    #[serde(default)]
    pub zones: HashMap<String, ZoneConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub config_file: PathBuf,
    config: ConfigFile,
}

impl Config {
    pub fn from_file(config_file: &Path) -> Result<Config, String> {
        let f = File::open(config_file)
            .map_err(|e| format!("Failed to open file at {}: {}", config_file.display(), e))?;

        let config: ConfigFile = serde_yaml::from_reader(f)
            .map_err(|e| format!("Failed to parse configuration: {}", e))?;

        Ok(Config {
            config_file: config_file.to_owned(),
            config,
        })
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        self.config_file.parent().unwrap().join(path)
    }

    pub fn default(config_file: &Path) -> Self {
        Config {
            config_file: config_file.to_owned(),
            config: ConfigFile::default(),
        }
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.config.server
    }

    pub fn docker_sources(&self) -> Iter<String, DockerConfig> {
        self.config.sources.docker.iter()
    }

    pub fn traefik_sources(&self) -> Iter<String, TraefikConfig> {
        self.config.sources.traefik.iter()
    }

    pub fn dhcp_sources(&self) -> Iter<String, DhcpConfig> {
        self.config.sources.dhcp.iter()
    }

    pub fn file_sources(&self) -> Iter<String, FileConfig> {
        self.config.sources.file.iter()
    }

    pub fn upstream(&self) -> &Option<UpstreamConfig> {
        &self.config.upstream
    }

    fn zone(&self, domain: Name) -> Zone {
        let name = domain.to_string();
        let fqdn = String::from(name.trim_end_matches('.'));

        let config = self.config.zones.get(&fqdn);
        let upstream_config = match config {
            Some(c) => {
                if c.authoratative {
                    None
                } else {
                    c.upstream.as_ref().or(self.config.upstream.as_ref())
                }
            }
            None => self.config.upstream.as_ref(),
        };

        let upstream = upstream_config.map(|c| Upstream::new(&domain.to_string(), c));

        Zone::new(domain, upstream)
    }

    pub fn zones(&self, records: &RecordSet) -> Vec<Zone> {
        let mut zones: HashMap<Name, Zone> = Default::default();

        for domain in self.config.zones.keys() {
            let name = Name::parse(&format!("{}.", domain), None).unwrap();
            let zone = self.zone(name.clone());
            zones.insert(name, zone);
        }

        for record in records {
            let domain = record.name.trim_to((record.name.num_labels() - 1).into());

            match zones.get_mut(&domain) {
                Some(zone) => zone.insert(record.clone()),
                None => {
                    let mut zone = self.zone(domain.clone());
                    zone.insert(record.clone());
                    zones.insert(domain, zone);
                }
            };
        }

        zones.into_values().collect()
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

            config = next_config;

            let actual_config = match config {
                Ok(ref config) => config.clone(),
                Err(ref e) => {
                    log::error!("{}", e);
                    Config::default(&config_file)
                }
            };

            if let Err(e) = sender.send(actual_config) {
                log::error!("Failed to send updated config: {}", e);
                return;
            }
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
