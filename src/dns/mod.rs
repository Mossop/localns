use std::{sync::Arc, time::Duration};

use hickory_server::ServerFuture;
use serde::Deserialize;
use tokio::{
    net::{TcpListener, UdpSocket},
    select,
    sync::{watch, Mutex},
};

mod handler;
mod record;
mod server;
mod upstream;

use crate::config::Config;

use self::{handler::Handler, server::Server};

pub use record::{Fqdn, RData, RDataConfig, Record, RecordSet, RecordSource};
pub use upstream::Upstream;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    port: Option<u16>,
}

async fn create_dns_server(config: &Config, server: Arc<Mutex<Server>>) -> ServerFuture<Handler> {
    let handler = Handler { server };

    let port = config.server.port.unwrap_or(53);
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

pub fn create_server(
    mut config_stream: watch::Receiver<Config>,
    mut record_stream: watch::Receiver<RecordSet>,
) {
    tokio::spawn(async move {
        let mut config = config_stream.borrow_and_update().clone();
        let mut records = record_stream.borrow_and_update().clone();

        let server = Arc::new(Mutex::new(Server::new(config.clone(), records.clone())));
        let mut dns_server = create_dns_server(&config, server.clone()).await;

        loop {
            select! {
                result = config_stream.changed() => {
                    if result.is_err() {
                        return;
                    }

                    let new_config = config_stream.borrow().clone();
                    {
                        let mut server = server.lock().await;
                        server.update_config(new_config.clone());
                    }

                    if config.server != new_config.server {
                        if let Err(e) = dns_server.block_until_done().await {
                            log::error!("Error waiting for DNS server to shutdown: {}", e);
                        }

                        dns_server = create_dns_server(&new_config, server.clone()).await;
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
                        let mut server = server.lock().await;
                        server.update_records(records.clone());
                    }
                }
            }
        }
    });
}
