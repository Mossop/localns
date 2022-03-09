use std::collections::HashSet;

use flexi_logger::Logger;
use localns::{config_stream, RecordSources, Server};
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();

    let mut config_stream = config_stream(&args);
    let config = config_stream.borrow_and_update().clone();

    let mut record_sources = RecordSources::new();
    record_sources.replace_sources(&config).await;
    let mut record_stream = record_sources.receiver();

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("Failed to register signal handler: {}", e))?;

    let mut server = Server::new(&config).await;

    loop {
        select! {
            result = config_stream.changed() => match result {
                Ok(_) => {
                    log::trace!("Saw updated configuration");
                    let config = config_stream.borrow().clone();
                    record_sources.replace_sources(&config).await;
                    server.update_config(&config).await;
                },
                Err(_) => {
                    log::trace!("Config stream ended");
                    break;
                },
            },
            result = record_stream.changed() => {
                let records = match result {
                    Ok(_) => record_stream.borrow().clone(),
                    Err(_) => HashSet::new(),
                };

                server.update_records(records).await;
            },
            _ = sigterm.recv() => {
                log::trace!("Saw SIGTERM");
                break;
            }
        }
    }

    record_sources.destroy().await;

    Ok(())
}

#[tokio::main]
async fn main() {
    let logger = match Logger::try_with_env_or_str("info") {
        Ok(logger) => logger,
        Err(e) => panic!("Failed to start logging: {}", e),
    };

    if let Err(e) = logger.start() {
        panic!("Failed to start logging: {}", e);
    }

    if let Err(e) = run().await {
        log::error!("{}", e);
    }
}
