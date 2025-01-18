use std::{
    collections::{HashMap, VecDeque},
    fmt, fs,
    path::Path,
    process,
};

use figment::{
    providers::{Env, Format, Yaml},
    value::{Uncased, UncasedStr},
    Figment,
};
use hickory_server::proto::{rr, rr::rdata::SOA};
use tracing::instrument;

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
            origin.name(),
            self.ttl,
            rr::RData::SOA(SOA::new(
                origin.child("ns").ok()?.name(),
                origin.child("hostmaster").ok()?.name(),
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
            if n.zone_of(name) {
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
    #[instrument(level = "debug", name = "config_parse", fields(config_file = %config_file.display()), err)]
    pub(crate) fn from_file(config_file: &Path) -> Result<Config, Error> {
        tracing::info!("Reading configuration");

        let config: file::ConfigFile = Figment::new()
            .join(Env::prefixed("LOCALNS_").map(map_env).lowercase(false))
            .join(Yaml::file_exact(config_file))
            .extract()?;

        if let Some(path) = config.pid_file {
            let id = process::id();
            if let Err(e) = fs::write(path.relative(), id.to_string()) {
                tracing::warn!(error=%e, "Failed to write PID file.");
            }
        }

        Ok(Config {
            server: config.server,
            api: config.api,
            sources: config.sources,
            zones: Zones::new(config.defaults, config.zones),
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::{
        config::{Config, ZoneConfigProvider},
        sources::docker,
        test::{fqdn, write_file},
    };

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn parse_config() {
        let temp = TempDir::new().unwrap();

        let config_file = temp.path().join("config.yml");
        write_file(
            &config_file,
            r#"
defaults:
  upstream: 10.10.14.250

sources:
  file:
    zones: zone.yaml
  dhcp:
    dnsmask:
      lease_file: dnsmasq.leases
      zone: home.clantownsend.com
  remote:
    other:
        url: https://other.local/
  docker:
    local: {}

zones:
  home.local: {}
  other.local:
    upstream: 10.10.15.250:5353
"#,
        )
        .await;

        let config = Config::from_file(&config_file).unwrap();

        let zone_config = config.zones.zone_config(&fqdn("nowhere.local"));

        assert!(!zone_config.authoritative);
        assert_eq!(zone_config.upstreams.len(), 1);
        assert_eq!(
            zone_config.upstreams.front().unwrap().config.address(53),
            "10.10.14.250:53"
        );

        let zone_config = config.zones.zone_config(&fqdn("www.other.local"));

        assert!(zone_config.authoritative);
        assert_eq!(zone_config.upstreams.len(), 2);
        assert_eq!(
            zone_config.upstreams.front().unwrap().config.address(53),
            "10.10.15.250:5353"
        );
        assert_eq!(
            zone_config.upstreams.get(1).unwrap().config.address(5324),
            "10.10.14.250:5324"
        );

        assert_eq!(config.sources.docker.len(), 1);
        let (name, docker_config) = config.sources.docker.iter().next().unwrap();
        assert_eq!(name, "local");
        assert!(matches!(docker_config, docker::DockerConfig::Local {}));
    }
}
