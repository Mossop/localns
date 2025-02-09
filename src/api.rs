use std::net::SocketAddr;

use actix_web::{dev, get, web, App, HttpServer, Responder};
use serde::{Deserialize, Serialize};

use crate::dns::store::{RecordStore, RecordStoreData};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ApiConfig {
    pub(crate) address: SocketAddr,
}

#[derive(Clone)]
struct AppData {
    record_store: RecordStore,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ApiRecords {
    pub(crate) server_version: String,
    #[serde(flatten)]
    pub(crate) store: RecordStoreData,
}

#[get("/v2/records")]
async fn v2_records(app_data: web::Data<AppData>) -> impl Responder {
    let api_records = ApiRecords {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        store: app_data.record_store.store_data().await,
    };

    web::Json(api_records)
}

fn create_server(config: &ApiConfig, app_data: AppData) -> Option<(dev::Server, u16)> {
    tracing::info!(address = %config.address, "Starting API server");

    let api_server = match HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_data.clone()))
            .service(v2_records)
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
    pub(crate) fn new(config: &ApiConfig, record_store: RecordStore) -> Option<Self> {
        let data = AppData { record_store };

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

    pub(crate) async fn shutdown(self) {
        self.api_server.stop(!cfg!(test)).await;
    }
}
