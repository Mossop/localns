use std::{fmt, net::SocketAddr, time::Instant};

use hickory_client::{
    client::AsyncClient,
    client::ClientHandle,
    op::{DnsResponse, ResponseCode},
    rr::{DNSClass, Name, RecordType},
    udp::UdpClientStream,
};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tracing::Level;

use crate::{event_lvl, util::Address, Error};

pub(crate) type UpstreamConfig = Address;

async fn connect_client(address: SocketAddr) -> Result<AsyncClient, Error> {
    let stream = UdpClientStream::<UdpSocket>::new(address);

    let client = AsyncClient::connect(stream);
    let (client, bg) = client.await?;
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
                tracing::error!(error = %e, "Unable to lookup nameserver");
                return None;
            }
        };

        let mut client = match connect_client(address).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e);
                return None;
            }
        };

        match client.query(name.clone(), query_class, query_type).await {
            Ok(response) => {
                let level = match response.response_code() {
                    ResponseCode::NoError | ResponseCode::NXDomain => Level::TRACE,
                    _ => Level::WARN,
                };

                let duration = Instant::now() - start;
                event_lvl!(
                    level,
                    id = id,
                    upstream = address.to_string(),
                    query = name.to_string(),
                    qtype = query_type.to_string(),
                    class = query_class.to_string(),
                    response_code = response.response_code().to_string(),
                    answers = response.answer_count(),
                    authorities = response.name_server_count(),
                    additionals = response.additional_count(),
                    rflags = response.flags().to_string(),
                    duration_ms = duration.as_millis(),
                    "Upstream request"
                );

                Some(response)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Upstream DNS server returned error");
                None
            }
        }
    }
}
