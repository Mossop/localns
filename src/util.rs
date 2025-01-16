use std::{
    fmt::{self, Display},
    hash::Hash,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use serde::Deserialize;

pub(crate) type Host = IpAddr;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Hash)]
#[serde(try_from = "String")]
pub(crate) struct Address {
    pub host: Host,
    pub port: Option<u16>,
}

impl Address {
    pub(crate) fn address(&self, default_port: u16) -> String {
        format!("{}:{}", self.host, self.port.unwrap_or(default_port))
    }

    pub(crate) fn to_socket_address(&self, default_port: u16) -> SocketAddr {
        SocketAddr::new(self.host, self.port.unwrap_or(default_port))
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

impl TryFrom<String> for Address {
    type Error = <SocketAddr as FromStr>::Err;

    fn try_from(host: String) -> Result<Self, Self::Error> {
        match SocketAddr::from_str(&host) {
            Ok(addr) => Ok(Self {
                host: addr.ip(),
                port: Some(addr.port()),
            }),
            Err(e) => {
                if let Ok(ip) = host.parse::<Host>() {
                    Ok(ip.into())
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl From<Host> for Address {
    fn from(host: Host) -> Self {
        Self { host, port: None }
    }
}

#[macro_export]
macro_rules! event_lvl {
    ($lvl:ident, $($arg:tt)+) => {
        match $lvl {
            ::tracing::Level::TRACE => ::tracing::trace!($($arg)+),
            ::tracing::Level::DEBUG => ::tracing::debug!($($arg)+),
            ::tracing::Level::INFO => ::tracing::info!($($arg)+),
            ::tracing::Level::WARN => ::tracing::warn!($($arg)+),
            ::tracing::Level::ERROR => ::tracing::error!($($arg)+),
        }
    };
}
