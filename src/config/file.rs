use std::{collections::HashMap, fmt};

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

pub(crate) fn deserialize_url<'de, D>(de: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    de.deserialize_str(UrlVisitor)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(super) struct ZoneConfig {
    #[serde(default)]
    pub upstream: Option<Upstream>,

    #[serde(default)]
    pub ttl: Option<u32>,

    #[serde(default)]
    pub authoritative: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ConfigFile {
    #[serde(default)]
    pub defaults: ZoneConfig,

    #[serde(default)]
    pub api: Option<ApiConfig>,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub sources: SourcesConfig,

    #[serde(default)]
    pub zones: HashMap<Fqdn, ZoneConfig>,
}
