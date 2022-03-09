use std::{collections::HashSet, sync::Arc, time::Duration};

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

async fn create_dns_server(config: &Config, catalog: Arc<Mutex<Catalog>>) -> ServerFuture<Handler> {
    let handler = Handler {
        catalog,
        upstream: config
            .upstream()
            .as_ref()
            .map(|c| Upstream::new("server", c)),
    };

    let port = config.server_config().port.unwrap_or(53);
    log::trace!("Server listening on port {}", port);

    let mut server = ServerFuture::new(handler);

    match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(socket) => server.register_socket(socket),
        Err(e) => log::error!("Unable to open UDP socket: {}", e),
    }

    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(socket) => server.register_listener(socket, Duration::from_millis(500)),
        Err(e) => log::error!("Unable to open TCP socket: {}", e),
    }

    server
}

async fn apply_records(
    config: &Config,
    catalog: &mut Catalog,
    current_zones: &mut HashSet<LowerName>,
    records: &RecordSet,
) {
    log::trace!("Updating server with records: {:?}", records);

    let mut previous_zones = current_zones.clone();

    for zone in config.zones(records) {
        current_zones.insert(zone.origin().clone());
        previous_zones.remove(&zone.origin().clone());

        catalog.upsert(zone.origin().clone(), Box::new(Arc::new(zone)));
    }

    for name in previous_zones {
        catalog.remove(&name);
        current_zones.remove(&name);
    }
}

pub fn create_server(
    mut config_stream: watch::Receiver<Config>,
    mut record_stream: watch::Receiver<RecordSet>,
) {
    tokio::spawn(async move {
        let mut config = config_stream.borrow_and_update().clone();
        let mut records = record_stream.borrow_and_update().clone();

        let mut catalog = Catalog::new();
        let mut current_zones = HashSet::new();
        apply_records(&config, &mut catalog, &mut current_zones, &records).await;

        let catalog = Arc::new(Mutex::new(catalog));

        let mut dns_server = create_dns_server(&config, catalog.clone()).await;

        loop {
            select! {
                result = config_stream.changed() => {
                    if result.is_err() {
                        return;
                    }

                    let new_config = config_stream.borrow().clone();

                    {
                        let mut catalog = catalog.lock().await;
                        apply_records(
                            &new_config,
                            &mut catalog,
                            &mut current_zones,
                            &records,
                        )
                        .await;
                    }

                    if config.server_config() != new_config.server_config() {
                        if let Err(e) = dns_server.block_until_done().await {
                            log::error!("Error waiting for DNS server to shutdown: {}", e);
                        }

                        dns_server = create_dns_server(&new_config, catalog.clone()).await;
                    }

                    config = new_config;
                },
                result = record_stream.changed() => {
                    if result.is_err() {
                        return;
                    }

                    let new_records = record_stream.borrow().clone();
                    if new_records != records {
                        records = new_records;

                        let mut catalog = catalog.lock().await;
                        apply_records(&config, &mut catalog, &mut current_zones, &records).await;
                    }
                }
            }
        }
    });
}
