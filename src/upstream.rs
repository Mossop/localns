use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;
use tokio::net::UdpSocket;
use trust_dns_server::client::{
    client::AsyncClient,
    client::ClientHandle,
    op::Query,
    rr::{rdata::SOA, Record},
    udp::UdpClientStream,
};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct UpstreamConfig {
    address: IpAddr,
    #[serde(default)]
    port: Option<u16>,
}

async fn connect_client(address: SocketAddr) -> Result<AsyncClient, String> {
    let stream = UdpClientStream::<UdpSocket>::new(address);

    let client = AsyncClient::connect(stream);
    let (client, bg) = client
        .await
        .map_err(|e| format!("Failed to connect to DNS server: {}", e))?;
    tokio::spawn(bg);

    Ok(client)
}

pub struct UpstreamResponse {
    pub answers: Vec<Record>,
    pub additionals: Vec<Record>,
    pub name_servers: Vec<Record>,
    pub soa: Option<SOA>,
}

#[derive(Clone, Debug)]
pub struct Upstream {
    name: String,
    config: UpstreamConfig,
}

impl Upstream {
    pub fn new(name: &str, config: &UpstreamConfig) -> Self {
        Self {
            name: name.to_owned(),
            config: config.clone(),
        }
    }

    pub async fn lookup(&self, query: Query) -> Option<UpstreamResponse> {
        log::trace!(
            "({}) Forwarding query for {} to upstream {}",
            self.name,
            query.name(),
            self.config.address
        );

        let mut client = match connect_client(SocketAddr::new(
            self.config.address,
            self.config.port.unwrap_or(53),
        ))
        .await
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("({}) {}", self.name, e);
                return None;
            }
        };

        match client
            .query(
                query.name().clone(),
                query.query_class(),
                query.query_type(),
            )
            .await
        {
            Ok(mut response) => Some(UpstreamResponse {
                answers: response.take_answers(),
                additionals: response.take_additionals(),
                name_servers: response.take_name_servers(),
                soa: response.soa(),
            }),
            Err(e) => {
                log::warn!("({}) Upstream DNS server returned error: {}", self.name, e);
                None
            }
        }
    }
}
