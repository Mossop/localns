use std::{fmt, net::SocketAddr};

use serde::Deserialize;
use tokio::net::UdpSocket;
use trust_dns_server::client::{
    client::AsyncClient,
    client::ClientHandle,
    op::DnsResponse,
    rr::{DNSClass, Name, RecordType},
    udp::UdpClientStream,
};

use crate::util::Address;

pub type UpstreamConfig = Address;

async fn connect_client(address: SocketAddr) -> Result<AsyncClient, String> {
    let stream = UdpClientStream::<UdpSocket>::new(address);

    let client = AsyncClient::connect(stream);
    let (client, bg) = client
        .await
        .map_err(|e| format!("Failed to connect to DNS server: {}", e))?;
    tokio::spawn(bg);

    Ok(client)
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "UpstreamConfig")]
pub struct Upstream {
    config: UpstreamConfig,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&format!("{}", self.config))
    }
}

impl From<UpstreamConfig> for Upstream {
    fn from(config: UpstreamConfig) -> Upstream {
        Upstream { config }
    }
}

impl Upstream {
    pub fn new(config: &UpstreamConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    pub async fn lookup(
        &self,
        name: &Name,
        query_class: DNSClass,
        query_type: RecordType,
    ) -> Option<DnsResponse> {
        let address = match self.config.to_socket_address(53) {
            Ok(addr) => addr,
            Err(e) => {
                log::error!("Unable to lookup nameserver: {}", e);
                return None;
            }
        };

        let mut client = match connect_client(address).await {
            Ok(c) => c,
            Err(e) => {
                log::error!("{}", e);
                return None;
            }
        };

        log::trace!(
            "Upstream request to {} for {} class {} type {}",
            address,
            name,
            query_class,
            query_type
        );

        match client.query(name.clone(), query_class, query_type).await {
            Ok(response) => Some(response),
            Err(e) => {
                log::warn!("Upstream DNS server returned error: {}", e);
                None
            }
        }
    }
}
