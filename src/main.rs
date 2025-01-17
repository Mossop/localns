use std::{
    env::{self, VarError},
    io,
    path::PathBuf,
    time::Duration,
};

use clap::Parser;
use localns::{Error, Server};
use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator, runtime, trace::TracerProvider, Resource,
};
use tokio::signal;
use tracing::{warn, Level};
use tracing_subscriber::{
    filter::Targets, layer::SubscriberExt, util::SubscriberInitExt, Layer, Registry,
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

fn init_logging() -> Result<Option<TracerProvider>, Error> {
    let telemetry_host = match env::var("LOCALNS_TELEMETRY") {
        Ok(host) => Some(host),
        Err(VarError::NotPresent) => None,
        Err(e) => {
            warn!("Invalid environment: {e}");
            None
        }
    };

    let default_level = if cfg!(debug_assertions) {
        Level::TRACE
    } else {
        Level::INFO
    };

    let formatter = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .pretty()
        .with_writer(io::stderr)
        .with_filter(Targets::new().with_target("localns", default_level));

    let registry = Registry::default().with(formatter);

    if let Some(telemetry_host) = telemetry_host {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let tracer_provider = TracerProvider::builder()
            .with_batch_exporter(
                SpanExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(telemetry_host)
                    .with_timeout(Duration::from_secs(3))
                    .build()?,
                runtime::Tokio,
            )
            .with_resource(Resource::new(vec![KeyValue::new(
                "service.name",
                "localns",
            )]))
            .build();

        let tracer = tracer_provider.tracer("localns");

        let filter = Targets::new().with_target("localns", Level::TRACE);

        let telemetry = tracing_opentelemetry::layer()
            .with_error_fields_to_exceptions(true)
            .with_tracked_inactivity(true)
            .with_tracer(tracer)
            .with_filter(filter);

        registry.with(telemetry).init();

        Ok(Some(tracer_provider))
    } else {
        registry.init();

        Ok(None)
    }
}

#[tokio::main]
async fn main() {
    let tracer_provider = match init_logging() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to initialise logging: {e}");
            None
        }
    };

    if let Err(e) = run().await {
        tracing::error!(error = %e, "Unexpected error");
    }

    if let Some(provider) = tracer_provider {
        provider.force_flush();
    }
}
