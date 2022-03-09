use std::net::SocketAddr;

use tokio::net::UdpSocket;
use trust_dns_server::client::{
    client::AsyncClient,
    client::ClientHandle,
    op::Query,
    rr::{rdata::SOA, Record},
    udp::UdpClientStream,
};

use crate::config::Address;

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

pub struct UpstreamResponse {
    pub authoritative: bool,
    pub recursion_available: bool,

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
        let address = match self.config.to_socket_address(53) {
            Ok(addr) => addr,
            Err(e) => {
                log::error!("({}) Unable to lookup nameserver: {}", self.name, e);
                return None;
            }
        };

        let mut client = match connect_client(address).await {
            Ok(c) => c,
            Err(e) => {
                log::error!("({}) {}", self.name, e);
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
                authoritative: response.authoritative(),
                recursion_available: response.recursion_available(),

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
