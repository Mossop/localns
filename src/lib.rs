mod config;
mod debounce;
mod docker;
mod rfc1035;

pub use config::{config_file, Config, ConfigStream};
pub use docker::{DockerState, DockerStateStream};
pub use rfc1035::{RecordData, ResourceRecord};

pub fn write_zone(config: &Config, state: &DockerState) -> Result<(), String> {
    log::trace!("Writing zone...");
    Ok(())
}
