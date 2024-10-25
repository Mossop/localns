use figment::{
    providers::{Env, Format, Yaml},
    value::{Uncased, UncasedStr},
    Figment,
};
use hickory_server::{proto::rr, proto::rr::rdata::SOA};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    path::Path,
};

use crate::{
    api::ApiConfig,
    dns::{Fqdn, ServerConfig, Upstream},
    sources::SourcesConfig,
    Error,
};

mod file;

pub(crate) use file::deserialize_url;

pub(crate) struct ZoneConfig {
    pub(crate) origin: Option<Fqdn>,
    pub(crate) upstreams: VecDeque<Upstream>,
    pub(crate) ttl: u32,
    pub(crate) authoritative: bool,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self {
            origin: None,
            upstreams: VecDeque::new(),
            ttl: 300,
            authoritative: false,
        }
    }
}

impl From<&file::DefaultZoneConfig> for ZoneConfig {
    fn from(defaults: &file::DefaultZoneConfig) -> Self {
        Self {
            origin: None,
            upstreams: VecDeque::from_iter(defaults.upstream.iter().cloned()),
            ttl: defaults.ttl.unwrap_or(300),
            authoritative: false,
        }
    }
}

impl ZoneConfig {
    pub(crate) fn soa(&self) -> Option<rr::Record> {
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

    fn apply_config(&mut self, origin: Fqdn, config: &file::PartialZoneConfig) {
        self.origin = Some(origin);

        if let Some(ref upstream) = config.config.upstream {
            self.upstreams.push_front(upstream.clone());
        }
        if let Some(ttl) = config.config.ttl {
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

        if !self.upstreams.is_empty() {
            let strings: Vec<String> = self.upstreams.iter().map(|u| format!("{u:?}")).collect();
            parts.push(format!("upstream={:?}", strings.join(",")));
        }

        f.pad(&format!("[{}]", parts.join(" ")))
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct Zones {
    defaults: file::DefaultZoneConfig,
    zones: Vec<(Fqdn, file::PartialZoneConfig)>,
}

impl Zones {
    fn new(
        defaults: file::DefaultZoneConfig,
        mut zones: HashMap<Fqdn, file::PartialZoneConfig>,
    ) -> Self {
        let mut zones: Vec<(Fqdn, file::PartialZoneConfig)> = zones.drain().collect();
        zones.sort_by(|(n1, _), (n2, _)| n1.cmp(n2));

        Self { defaults, zones }
    }
}

pub(crate) trait ZoneConfigProvider {
    fn zone_config(&self, fqdn: &Fqdn) -> ZoneConfig;
}

impl ZoneConfigProvider for Zones {
    fn zone_config(&self, name: &Fqdn) -> ZoneConfig {
        let mut config = ZoneConfig::from(&self.defaults);

        for (n, c) in &self.zones {
            if n.is_parent(name) {
                config.apply_config(n.clone(), c);
            }
        }

        config
    }
}

fn map_env(key: &UncasedStr) -> Uncased<'_> {
    key.as_str()
        .split('_')
        .enumerate()
        .fold(String::new(), |mut key, (idx, part)| {
            if idx == 0 {
                key.push_str(&part.to_lowercase());
            } else {
                key.push_str(&part[0..1].to_uppercase());
                key.push_str(&part[1..].to_lowercase());
            }

            key
        })
        .into()
}

#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) struct Config {
    pub server: ServerConfig,
    pub api: Option<ApiConfig>,
    pub sources: SourcesConfig,
    pub(crate) zones: Zones,
}

impl Config {
    pub(crate) fn from_file(config_file: &Path) -> Result<Config, Error> {
        tracing::info!(config_file = %config_file.display(), "Reading configuration");

        let config: file::ConfigFile = Figment::new()
            .join(Env::prefixed("LOCALNS_").map(map_env).lowercase(false))
            .join(Yaml::file_exact(config_file))
            .extract()?;

        Ok(Config {
            server: config.server,
            api: config.api,
            sources: config.sources,
            zones: Zones::new(config.defaults, config.zones),
        })
    }
}
