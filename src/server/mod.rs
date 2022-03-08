use std::{collections::HashSet, mem::replace, sync::Arc};

use serde::Deserialize;
use tokio::{net::UdpSocket, sync::Mutex};
use trust_dns_server::{
    authority::{Authority, Catalog},
    client::rr::LowerName,
    ServerFuture,
};

mod handler;
mod zone;

use crate::{config::Config, record::RecordSet, upstream::Upstream};

use self::handler::Handler;

pub use zone::Zone;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    port: Option<u16>,
}

pub struct Server {
    config: Config,
    records: RecordSet,
    current_zones: HashSet<LowerName>,

    dns_server: ServerFuture<Handler>,

    catalog: Arc<Mutex<Catalog>>,
}

async fn apply_config(server: &mut ServerFuture<Handler>, config: &ServerConfig) {
    let port = config.port.unwrap_or(53);
    log::trace!("Server listening on port {}", port);

    match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(socket) => server.register_socket(socket),
        Err(e) => log::error!("Unable to open DNS socket: {}", e),
    }
}

async fn apply_records(
    config: &Config,
    catalog: &mut Catalog,
    mut previous_names: HashSet<LowerName>,
    records: RecordSet,
) -> HashSet<LowerName> {
    log::trace!("Updating server with records: {:?}", records);

    let mut names = HashSet::new();

    for zone in config.zones(records) {
        names.insert(zone.origin().clone());
        previous_names.remove(&zone.origin().clone());

        catalog.upsert(zone.origin().clone(), Box::new(Arc::new(zone)));
    }

    names
}

impl Server {
    pub async fn new(config: &Config) -> Self {
        let mut catalog = Catalog::new();
        let names = apply_records(config, &mut catalog, HashSet::new(), RecordSet::new()).await;
        let locked = Arc::new(Mutex::new(catalog));

        let handler = Handler {
            catalog: locked.clone(),
            upstream: config
                .upstream()
                .as_ref()
                .map(|c| Upstream::new("server", c)),
        };

        let mut dns_server = ServerFuture::new(handler);
        apply_config(&mut dns_server, config.server_config()).await;

        Server {
            config: config.clone(),
            records: RecordSet::new(),
            current_zones: names,
            dns_server,
            catalog: locked,
        }
    }

    pub async fn update_config(&mut self, config: &Config) {
        let mut catalog = self.catalog.lock().await;
        let names = apply_records(
            config,
            &mut catalog,
            self.current_zones.clone(),
            self.records.clone(),
        )
        .await;
        self.current_zones = names;

        if self.config.server_config() == config.server_config() {
            return;
        }

        self.config = config.to_owned();

        let handler = Handler {
            catalog: self.catalog.clone(),
            upstream: config
                .upstream()
                .as_ref()
                .map(|c| Upstream::new("server", c)),
        };

        let dns_server = ServerFuture::new(handler);
        let old_server = replace(&mut self.dns_server, dns_server);

        if let Err(e) = old_server.block_until_done().await {
            log::error!("Error waiting for DNS server to shutdown: {}", e);
        }

        apply_config(&mut self.dns_server, self.config.server_config()).await;
    }

    pub async fn update_records(&mut self, records: RecordSet) {
        if self.records == records {
            return;
        }

        self.records = records.clone();

        let mut catalog = self.catalog.lock().await;
        let names = apply_records(
            &self.config,
            &mut catalog,
            self.current_zones.clone(),
            records,
        )
        .await;
        self.current_zones = names;
    }
}
