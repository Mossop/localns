use std::{
    fmt::{self, Display},
    hash::Hash,
    net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
};

use serde::Deserialize;

use crate::dns::Fqdn;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Host {
    Name(Fqdn),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

impl Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Name(st) => f.pad(st.to_string().trim_end_matches('.')),
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
#[serde(from = "String")]
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
