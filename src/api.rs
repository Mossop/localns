use std::net::SocketAddr;

use actix_web::{dev, get, web, App, HttpServer, Responder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    dns::{store::RecordStore, Record},
    sources::SourceRecords,
    ServerId,
};

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ApiConfig {
    pub(crate) address: SocketAddr,
}

#[derive(Clone)]
struct AppData {
    server_id: ServerId,
    record_store: RecordStore,
}

#[get("/records")]
async fn records(app_data: web::Data<AppData>) -> impl Responder {
    let records: Vec<Record> = app_data
        .record_store
        .resolve_source_records()
        .await
        .into_iter()
        .filter_map(|source_records| {
            if source_records.source_id.server_id == app_data.server_id {
                Some(source_records.records)
            } else {
                None
            }
        })
        .flatten()
        .collect();

    web::Json(records)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ApiRecords {
    pub(crate) server_id: ServerId,
    pub(crate) server_version: String,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) source_records: Vec<SourceRecords>,
}

#[get("/v2/records")]
async fn v2_records(app_data: web::Data<AppData>) -> impl Responder {
    let source_records = app_data.record_store.resolve_source_records().await;

    let api_records = ApiRecords {
        server_id: app_data.server_id,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: Utc::now(),
        source_records,
    };

    web::Json(api_records)
}

fn create_server(config: &ApiConfig, app_data: AppData) -> Option<(dev::Server, u16)> {
    tracing::info!(address = %config.address, "Starting API server");

    let api_server = match HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_data.clone()))
            .service(records)
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
    pub(crate) fn new(
        config: &ApiConfig,
        server_id: ServerId,
        record_store: RecordStore,
    ) -> Option<Self> {
        let data = AppData {
            server_id,
            record_store,
        };

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
