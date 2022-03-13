use std::{net::SocketAddr, sync::Arc};

use actix_web::{dev::Server, get, web, App, HttpServer, Responder};
use serde::Deserialize;
use tokio::{
    select,
    sync::{watch, Mutex},
};

use crate::{
    config::Config,
    dns::{Record, RecordSet, RecordSource},
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiConfig {
    address: SocketAddr,
}

struct AppState {
    records: Vec<Record>,
}

type AppData = Arc<Mutex<AppState>>;

#[get("/records")]
async fn requests(data: web::Data<AppData>) -> impl Responder {
    let state = data.lock().await;

    web::Json(state.records.clone())
}

fn create_server(config: Config, state: AppData) -> Option<Server> {
    let config = config.api?;
    log::trace!("Starting API server at {}", config.address);

    let server = match HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .service(requests)
    })
    .disable_signals()
    .bind(config.address)
    {
        Ok(server) => server,
        Err(e) => {
            log::error!("Failed to create API server: {}", e);
            return None;
        }
    };

    Some(server.run())
}

fn local_records(records: &RecordSet) -> Vec<Record> {
    records
        .records()
        .filter(|r| r.source == RecordSource::Local)
        .cloned()
        .collect()
}

pub fn create_api_server(
    mut config_stream: watch::Receiver<Config>,
    mut record_stream: watch::Receiver<RecordSet>,
) {
    tokio::spawn(async move {
        loop {
            let config = config_stream.borrow_and_update().clone();
            let state: AppData = Arc::new(Mutex::new(AppState {
                records: local_records(&record_stream.borrow_and_update()),
            }));

            let server = match create_server(config, state.clone()) {
                Some(server) => server,
                None => {
                    // Wait for new config and try again.
                    if config_stream.changed().await.is_err() {
                        // Bail out on error in the stream.
                        return;
                    }
                    continue;
                }
            };

            let handle = server.handle();
            tokio::spawn(server);

            loop {
                select! {
                    result = config_stream.changed() => {
                        handle.stop(true).await;

                        if result.is_err() {
                            return;
                        }
                        break;
                    },
                    result = record_stream.changed() => {
                        if result.is_err() {
                            handle.stop(true).await;
                            return;
                        }

                        let mut locked = state.lock().await;
                        locked.records = local_records(&record_stream
                            .borrow_and_update());
                    }
                }
            }
        }
    });
}
