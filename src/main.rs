use docker_dns::{config_file, write_zone, Config, ConfigStream, DockerState, DockerStateStream};
use flexi_logger::Logger;
use futures::StreamExt;
use tokio::select;

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();

    let config_file = config_file(args.get(1).cloned());
    log::info!("Reading configuration from {}", config_file.display());

    let mut config_stream = ConfigStream::new(&config_file);
    let mut docker_stream = DockerStateStream::new(config_stream.clone());

    let mut config: Config = config_stream.config.clone();
    let mut docker_state: Option<DockerState> = None;

    loop {
        select! {
            next = config_stream.next() => match next {
                Some(new_config) => config = new_config,
                None => break,
            },
            next = docker_stream.next() => match next {
                Some(new_state) => docker_state = new_state,
                None => break,
            }
        }

        if let Some(state) = &docker_state {
            if let Err(e) = write_zone(&config, state) {
                log::error!("Failed to write zone: {}", e);
            }
        }
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
