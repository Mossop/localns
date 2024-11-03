use std::{collections::HashMap, fmt};

use figment::value::magic::RelativePathBuf;
use reqwest::Url;
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};

use crate::{
    api::ApiConfig,
    dns::{Fqdn, ServerConfig, Upstream},
    sources::SourcesConfig,
};

struct UrlVisitor;

impl Visitor<'_> for UrlVisitor {
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

pub(crate) fn deserialize_url<'de, D>(de: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    de.deserialize_str(UrlVisitor)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(super) struct DefaultZoneConfig {
    #[serde(default)]
    pub(super) upstream: Option<Upstream>,

    #[serde(default)]
    pub(super) ttl: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(super) struct PartialZoneConfig {
    #[serde(flatten)]
    pub(super) config: DefaultZoneConfig,

    #[serde(default)]
    pub(super) authoritative: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ConfigFile {
    #[serde(default)]
    pub(super) pid_file: Option<RelativePathBuf>,

    #[serde(default)]
    pub(super) defaults: DefaultZoneConfig,

    #[serde(default)]
    pub(super) api: Option<ApiConfig>,

    #[serde(default)]
    pub(super) server: ServerConfig,

    #[serde(default)]
    pub(super) sources: SourcesConfig,

    #[serde(default)]
    pub(super) zones: HashMap<Fqdn, PartialZoneConfig>,
}
