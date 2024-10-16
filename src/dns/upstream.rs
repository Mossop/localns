use std::{fmt, net::SocketAddr};

use hickory_client::{
    client::AsyncClient,
    client::ClientHandle,
    op::DnsResponse,
    rr::{DNSClass, Name, RecordType},
    udp::UdpClientStream,
};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tracing::{instrument, Span};

use crate::{util::Address, Error};

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
pub(crate) struct Upstream {
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
    #[instrument(fields(
        lookup.upstream = %self.config,
        lookup.name = %name,
        lookup.query_class = %query_class,
        lookup.query_type = %query_type,
        lookup.response_code,
    ), skip_all)]
    pub(crate) async fn lookup(
        &self,
        name: &Name,
        query_class: DNSClass,
        query_type: RecordType,
    ) -> Option<DnsResponse> {
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

        let result = client.query(name.clone(), query_class, query_type).await;

        match result {
            Ok(response) => {
                let span = Span::current();
                span.record("lookup.response_code", response.response_code().to_string());
                Some(response)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Upstream DNS server returned error");
                None
            }
        }
    }
}
