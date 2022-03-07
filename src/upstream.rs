use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;
use tokio::net::TcpStream;
use trust_dns_server::{
    client::{
        client::AsyncClient,
        client::ClientHandle,
        op::Query,
        rr::{rdata::SOA, Record},
        tcp::TcpClientStream,
    },
    proto::iocompat::AsyncIoTokioAsStd,
};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct UpstreamConfig {
    address: IpAddr,
    #[serde(default)]
    port: Option<u16>,
}

async fn connect_client(address: SocketAddr) -> AsyncClient {
    let (stream, sender) = TcpClientStream::<AsyncIoTokioAsStd<TcpStream>>::new(address);

    let client = AsyncClient::new(stream, sender, None);
    let (client, bg) = client.await.expect("connection failed");
    tokio::spawn(bg);

    client
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
        log::debug!(
            "({}) Forwarding query for {} upstream",
            self.name,
            query.name()
        );

        let mut client = connect_client(SocketAddr::new(
            self.config.address,
            self.config.port.unwrap_or(53),
        ))
        .await;

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
