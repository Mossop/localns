use std::{mem::replace, net::Ipv4Addr, sync::Arc};

use serde::Deserialize;
use tokio::{net::UdpSocket, sync::Mutex};
use trust_dns_server::{
    authority::{Authority, Catalog},
    client::rr::{Name, RData, Record},
    resolver::TokioAsyncResolver,
    ServerFuture,
};

mod authority;
mod handler;

use crate::rfc1035::RecordSet;

use self::{authority::SemiAuthoritativeAuthority, handler::Handler};

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct ServerConfig {}

pub struct Server {
    config: ServerConfig,
    records: RecordSet,
    dns_server: ServerFuture<Handler>,

    catalog: Arc<Mutex<Catalog>>,
}

async fn apply_config(server: &mut ServerFuture<Handler>, _config: &ServerConfig) {
    match UdpSocket::bind("0.0.0.0:53").await {
        Ok(socket) => server.register_socket(socket),
        Err(e) => log::error!("Unable to open DNS socket: {}", e),
    }
}

async fn apply_records(catalog: &mut Catalog, _records: RecordSet) {
    let origin = Name::parse("oxymoronical.com.", None).unwrap();
    let mut authority = SemiAuthoritativeAuthority::new(origin.clone()).await;

    let record = Record::from_rdata(
        Name::parse("www", Some(&origin)).unwrap(),
        300,
        RData::A(Ipv4Addr::new(10, 10, 10, 10)),
    );
    authority.insert(record);

    catalog.upsert(authority.origin().clone(), Box::new(Arc::new(authority)));
}

impl Server {
    pub async fn new(config: &ServerConfig) -> Self {
        let mut catalog = Catalog::new();
        apply_records(&mut catalog, RecordSet::new()).await;
        let locked = Arc::new(Mutex::new(catalog));

        let handler = Handler {
            catalog: locked.clone(),
            resolver: TokioAsyncResolver::tokio_from_system_conf().unwrap(),
        };

        let mut dns_server = ServerFuture::new(handler);
        apply_config(&mut dns_server, config).await;

        Server {
            config: config.to_owned(),
            records: RecordSet::new(),
            dns_server,
            catalog: locked,
        }
    }

    pub async fn update_config(&mut self, config: &ServerConfig) {
        if self.config == *config {
            return;
        }

        self.config = config.to_owned();

        let handler = Handler {
            catalog: self.catalog.clone(),
            resolver: TokioAsyncResolver::tokio_from_system_conf().unwrap(),
        };

        let dns_server = ServerFuture::new(handler);
        let old_server = replace(&mut self.dns_server, dns_server);

        if let Err(e) = old_server.block_until_done().await {
            log::error!("Error waiting for DNS server to shutdown: {}", e);
        }

        apply_config(&mut self.dns_server, config).await;
    }

    pub async fn update_records(&mut self, records: RecordSet) {
        if self.records == records {
            return;
        }

        let mut catalog = self.catalog.lock().await;
        apply_records(&mut catalog, records).await;
    }
}