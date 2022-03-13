use std::{fmt, net::SocketAddr, time::Instant};

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
        id: u16,
        name: &Name,
        query_class: DNSClass,
        query_type: RecordType,
    ) -> Option<DnsResponse> {
        let start = Instant::now();

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

        match client.query(name.clone(), query_class, query_type).await {
            Ok(response) => {
                let duration = Instant::now() - start;
                log::debug!("({id}) Upstream UDP://{addr} {query}:{qtype}:{class} response:{code:?} rr:{answers}/{authorities}/{additionals} rflags:{rflags} ms:{duration}",
            id = id,
            addr = address,
            query = name,
            qtype = query_type,
            class = query_class,
            code = response.response_code(),
            answers = response.answer_count(),
            authorities = response.name_server_count(),
            additionals = response.additional_count(),
            rflags = response.flags(),
            duration = duration.as_millis(),
        );

                Some(response)
            }
            Err(e) => {
                log::warn!("Upstream DNS server returned error: {}", e);
                None
            }
        }
    }
}
