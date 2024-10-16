use std::{sync::Arc, time::Duration};

use hickory_server::ServerFuture;
use serde::Deserialize;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::RwLock,
};

mod handler;
mod record;
mod server;
mod upstream;

use crate::config::Zones;

use self::handler::Handler;

pub(crate) use record::{Fqdn, RData, RDataConfig, Record, RecordSet, RecordSource};
pub(crate) use upstream::Upstream;

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ServerConfig {
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerState {
    pub(crate) records: RecordSet,
    pub(crate) zones: Zones,
}

pub(crate) struct DnsServer {
    server_state: Arc<RwLock<ServerState>>,
    server: ServerFuture<Handler>,
}

impl DnsServer {
    pub(crate) async fn new(
        server_config: &ServerConfig,
        server_state: Arc<RwLock<ServerState>>,
    ) -> Self {
        Self {
            server_state: server_state.clone(),
            server: Self::build_server(server_config, server_state).await,
        }
    }

    pub(crate) async fn shutdown(&mut self) {
        tracing::debug!("Shutting down DNS service");

        if let Err(e) = self.server.shutdown_gracefully().await {
            tracing::error!(error = %e, "Failure while shutting down DNS server.");
        }
    }

    pub(crate) async fn restart(&mut self, server_config: &ServerConfig) {
        tracing::debug!("Restarting DNS service");

        if let Err(e) = self.server.block_until_done().await {
            tracing::error!(error = %e, "Failure while shutting down DNS server.");
        }

        self.server = Self::build_server(server_config, self.server_state.clone()).await;
    }

    async fn build_server(
        server_config: &ServerConfig,
        server_state: Arc<RwLock<ServerState>>,
    ) -> ServerFuture<Handler> {
        let handler = Handler { server_state };

        let port = server_config.port.unwrap_or(53);

        let mut server = ServerFuture::new(handler);

        match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                tracing::info!("Server listening on udp://0.0.0.0:{}", port);
                server.register_socket(socket);
            }
            Err(e) => tracing::error!(error = %e, "Unable to open UDP socket"),
        }

        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                tracing::info!("Server listening on tcp://0.0.0.0:{}", port);
                server.register_listener(socket, Duration::from_millis(500));
            }
            Err(e) => tracing::error!(error = %e, "Unable to open TCP socket"),
        }

        server
    }
}
