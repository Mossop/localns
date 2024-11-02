use std::{net::SocketAddr, sync::Arc};

use actix_web::{dev, get, web, App, HttpServer, Responder};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::dns::{Record, RecordSet, RecordSource};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ApiConfig {
    pub(crate) address: SocketAddr,
}

type AppData = Arc<RwLock<RecordSet>>;

#[get("/records")]
async fn records(server: web::Data<AppData>) -> impl Responder {
    let records: Vec<Record> = {
        server
            .read()
            .await
            .records()
            .filter(|r| r.source == RecordSource::Local)
            .cloned()
            .collect()
    };

    web::Json(records)
}

fn create_server(config: &ApiConfig, app_data: AppData) -> Option<(dev::Server, u16)> {
    tracing::trace!(address = %config.address, "Starting API server");

    let api_server = match HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_data.clone()))
            .service(records)
    })
    .disable_signals()
    .bind(config.address)
    {
        Ok(server) => server,
        Err(e) => {
            tracing::error!(error=%e, "Failed to create API server");
            return None;
        }
    };

    let port = api_server.addrs().first().unwrap().port();

    Some((api_server.run(), port))
}

pub(crate) struct ApiServer {
    #[cfg(test)]
    pub(crate) port: u16,
    api_server: dev::ServerHandle,
}

impl ApiServer {
    pub(crate) fn new(config: &ApiConfig, data: AppData) -> Option<Self> {
        create_server(config, data).map(|(api_server, _port)| {
            let handle = api_server.handle();
            tokio::spawn(api_server);

            Self {
                #[cfg(test)]
                port: _port,
                api_server: handle,
            }
        })
    }

    pub(crate) async fn shutdown(&self) {
        self.api_server.stop(true).await;
    }
}
