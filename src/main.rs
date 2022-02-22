use std::path::PathBuf;

use docker_dns::{Config, DockerListener};
use flexi_logger::Logger;
use tokio::{fs::read_to_string, sync::mpsc::channel};

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        return Err(format!(
            "Expected a configuration file command line argument."
        ));
    }

    let path = PathBuf::from(args[1].clone());
    let config_str = read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read config file: {}", e))?;

    let config = Config::from_str(&config_str)?;

    let (sender, mut receiver) = channel(32);
    DockerListener::new(&config, sender);

    while let Some(containers) = receiver.recv().await {
        log::info!("Received {} containers", containers.len());
    }

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
