use std::{
    fmt::{self, Display},
    hash::Hash,
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
};

use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Host {
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

impl Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Ipv4(ip) => f.pad(&ip.to_string()),
            Host::Ipv6(ip) => f.pad(&ip.to_string()),
        }
    }
}

impl TryFrom<&str> for Host {
    type Error = <IpAddr as FromStr>::Err;

    fn try_from(host: &str) -> Result<Self, Self::Error> {
        let ip = IpAddr::from_str(host)?;
        Ok(ip.into())
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

    pub(crate) fn to_socket_address(
        &self,
        default_port: u16,
    ) -> Result<SocketAddr, AddrParseError> {
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

impl TryFrom<String> for Address {
    type Error = <SocketAddr as FromStr>::Err;

    fn try_from(host: String) -> Result<Self, Self::Error> {
        match SocketAddr::from_str(&host) {
            Ok(addr) => Ok(Self {
                host: addr.ip().into(),
                port: Some(addr.port()),
            }),
            Err(e) => {
                if let Ok(ip) = host.parse::<Ipv4Addr>() {
                    Ok(Host::from(ip).into())
                } else if let Ok(ip) = host.parse::<Ipv6Addr>() {
                    Ok(Host::from(ip).into())
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
