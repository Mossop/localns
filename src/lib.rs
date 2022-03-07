mod backoff;
mod config;
mod debounce;
mod rfc1035;
mod server;
mod sources;
mod watcher;

use std::{
    fs::File,
    io::{self, BufWriter, Write},
};

pub use config::config_stream;
use config::Config;
use rfc1035::Zone;
pub use server::Server;
pub use sources::RecordSources;

fn root_zone_write(config: &Config, writer: &mut dyn io::Write) -> io::Result<()> {
    let mut buffer = Vec::new();
    let buf = &mut buffer;

    writeln!(buf, ". {{")?;
    if let Some(upstream) = config.upstream() {
        upstream.write(buf)?;
    }
    writeln!(buf, "}}")?;

    writer.write_all(&buffer)
}

pub fn write_root_zone(config: &Config) -> Result<(), String> {
    log::trace!("Writing root core file");

    let file = File::create(config.zone_path("root.core"))
        .map_err(|e| format!("Failed to open file: {}", e))?;
    let mut writer = BufWriter::new(file);

    root_zone_write(config, &mut writer).map_err(|e| format!("Failed to write zone: {}", e))?;

    writer
        .flush()
        .map_err(|e| format!("Failed to write zone: {}", e))
}

pub fn write_zone(config: &Config, zone: &Zone) -> Result<(), String> {
    log::trace!("Writing {} zone", zone.domain.non_absolute());

    let zone_file_name = config.zone_path(&format!("{}zone", zone.domain));
    let file = File::create(&zone_file_name).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut writer = BufWriter::new(file);
    zone.write_zone(&mut writer)
        .map_err(|e| format!("Failed to write zone file: {}", e))?;

    let file_name = format!("{}core", zone.domain);
    let file = File::create(file_name).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut writer = BufWriter::new(file);
    zone.write_core(&zone_file_name, &mut writer)
        .map_err(|e| format!("Failed to write core file: {}", e))?;

    writer
        .flush()
        .map_err(|e| format!("Failed to write zone: {}", e))
}
