use docker_dns::{config_file, config_stream, write_zone, RecordSources};
use flexi_logger::Logger;
use futures::StreamExt;
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();

    let config_file = config_file(args.get(1).cloned());
    log::info!("Reading configuration from {}", config_file.display());

    let mut config_stream = config_stream(&config_file);
    let mut config = match config_stream.next().await {
        Some(config) => config,
        None => return Ok(()),
    };

    log::trace!("Read initial configuration");

    let mut record_sources = RecordSources::from_config(config.sources.clone()).await;
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("Failed to register signal handler: {}", e))?;

    loop {
        select! {
            next = config_stream.next() => match next {
                Some(new_config) => {
                    log::trace!("Saw updated configuration");
                    config = new_config;
                    record_sources.destroy();
                    record_sources = RecordSources::from_config(config.sources.clone()).await;
                },
                None => {
                    log::trace!("Config stream ended");
                    break;
                },
            },
            Some(records) = record_sources.next() => {
                if let Err(e) = write_zone(&config, records) {
                    log::error!("Failed to write zone: {}", e);
                }
            }
            _ = sigterm.recv() => {
                log::trace!("Saw SIGTERM");
                break;
            }
        }
    }

    record_sources.destroy();

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
