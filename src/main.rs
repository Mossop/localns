use flexi_logger::Logger;
use localns::{config_stream, create_server, RecordSources};
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();

    let mut config_stream = config_stream(&args);
    let mut record_sources = RecordSources::new();

    create_server(config_stream.clone(), record_sources.receiver());

    record_sources
        .replace_sources(&config_stream.borrow_and_update())
        .await;

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("Failed to register signal handler: {}", e))?;

    loop {
        select! {
            result = config_stream.changed() => match result {
                Ok(_) => {
                    log::trace!("Saw updated configuration");
                    let config = config_stream.borrow().clone();
                    record_sources.replace_sources(&config).await;
                },
                Err(_) => {
                    log::trace!("Config stream ended");
                    break;
                },
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
