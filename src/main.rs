use std::io;

use clap::Parser;
use localns::{config_stream, create_api_server, create_server, RecordSources};
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};
use tracing_subscriber::{
    layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

#[derive(Parser)]
#[clap(author, version)]
struct CliArgs {
    config: Option<String>,
}

async fn run() -> Result<(), String> {
    let args = CliArgs::parse();

    let mut config_stream = config_stream(args.config.as_deref());
    let mut record_sources = RecordSources::new();

    create_server(config_stream.clone(), record_sources.receiver());
    create_api_server(config_stream.clone(), record_sources.receiver());

    record_sources
        .replace_sources(&config_stream.borrow_and_update())
        .await;

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("Failed to register signal handler: {}", e))?;

    loop {
        select! {
            result = config_stream.changed() => match result {
                Ok(_) => {
                    let config = config_stream.borrow().clone();
                    record_sources.replace_sources(&config).await;
                },
                Err(_) => {
                    tracing::debug!("Config stream ended");
                    break;
                },
            },
            _ = sigterm.recv() => {
                tracing::debug!("Saw SIGTERM");
                break;
            }
        }
    }

    record_sources.destroy().await;

    Ok(())
}

#[tokio::main]
async fn main() {
    let formatter = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .pretty()
        .with_writer(io::stderr)
        .with_filter(EnvFilter::from_default_env());

    Registry::default().with(formatter).init();

    if let Err(e) = run().await {
        tracing::error!("{}", e);
    }
}
