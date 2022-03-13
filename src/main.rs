use std::io;

use flexi_logger::{style, DeferredNow, Logger, Record};
use localns::{config_stream, create_api_server, create_server, RecordSources};
use time::{format_description::FormatItem, macros::format_description};
use tokio::{
    select,
    signal::unix::{signal, SignalKind},
};

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();

    let mut config_stream = config_stream(&args);
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
                    log::debug!("Config stream ended");
                    break;
                },
            },
            _ = sigterm.recv() => {
                log::debug!("Saw SIGTERM");
                break;
            }
        }
    }

    record_sources.destroy().await;

    Ok(())
}

pub const TIME_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

pub fn log_format(
    w: &mut dyn io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> Result<(), io::Error> {
    let level = record.level();
    write!(
        w,
        "[{}] {} [{}] {}",
        style(level).paint(now.format(TIME_FORMAT)),
        style(level).paint(format!("{:5}", level.to_string())),
        record.module_path().unwrap_or("<unnamed>"),
        style(level).paint(&record.args().to_string())
    )
}

#[tokio::main]
async fn main() {
    if let Err(e) =
        Logger::try_with_env_or_str("info").and_then(|logger| logger.format(log_format).start())
    {
        panic!("Failed to start logging: {}", e);
    }

    if let Err(e) = run().await {
        log::error!("{}", e);
    }
}
