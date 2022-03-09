use std::{collections::HashSet, mem::replace, sync::Arc, time::Duration};

use serde::Deserialize;
use tokio::{
    net::{TcpListener, UdpSocket},
    select,
    sync::{watch, Mutex},
};
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
        Err(e) => log::error!("Unable to open UDP socket: {}", e),
    }

    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(socket) => server.register_listener(socket, Duration::from_millis(500)),
        Err(e) => log::error!("Unable to open TCP socket: {}", e),
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

    for name in previous_names {
        catalog.remove(&name);
    }

    names
}

impl Server {
    pub fn create(
        mut config_stream: watch::Receiver<Config>,
        mut record_stream: watch::Receiver<RecordSet>,
    ) {
        let config = config_stream.borrow_and_update().clone();
        let records = record_stream.borrow_and_update().clone();

        tokio::spawn(async move {
            let mut catalog = Catalog::new();
            let names = apply_records(&config, &mut catalog, HashSet::new(), records).await;
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

            let mut server = Server {
                config: config.clone(),
                records: RecordSet::new(),
                current_zones: names,
                dns_server,
                catalog: locked,
            };

            loop {
                select! {
                    result = config_stream.changed() => {
                        if result.is_err() {
                            return;
                        }

                        let config = config_stream.borrow().clone();
                        server.update_config(&config).await;
                    },
                    result = record_stream.changed() => {
                        if result.is_err() {
                            return;
                        }

                        let records = record_stream.borrow().clone();
                        server.update_records(records).await;
                    }
                }
            }
        });
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
