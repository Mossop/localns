use std::{env, io, path::PathBuf};

use clap::Parser;
use localns::{Error, Server};
use tokio::signal;
use tracing_subscriber::{
    filter::Builder, layer::SubscriberExt, util::SubscriberInitExt, Layer, Registry,
};

#[derive(Parser)]
#[clap(author, version)]
struct CliArgs {
    config: Option<String>,
}

fn config_file(arg: Option<&str>) -> PathBuf {
    if let Some(str) = arg {
        PathBuf::from(str).canonicalize().unwrap()
    } else if let Ok(value) = env::var("LOCALNS_CONFIG") {
        PathBuf::from(value).canonicalize().unwrap()
    } else {
        PathBuf::from("config.yaml").canonicalize().unwrap()
    }
}

async fn wait_for_termination() {
    signal::ctrl_c().await.unwrap();
}

async fn run() -> Result<(), Error> {
    let args = CliArgs::parse();
    let config_path = config_file(args.config.as_deref());
    let server = Server::new(&config_path).await?;

    wait_for_termination().await;

    server.shutdown().await;

    Ok(())
}

#[tokio::main]
async fn main() {
    let env_filter = Builder::default()
        .with_default_directive("localns=trace".parse().unwrap())
        .from_env_lossy();

    let formatter = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .pretty()
        .with_writer(io::stderr)
        .with_filter(env_filter);

    Registry::default().with(formatter).init();

    if let Err(e) = run().await {
        tracing::error!(error = %e, "Unexpected error");
    }
}
