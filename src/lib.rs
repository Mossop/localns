mod config;
mod debounce;
mod docker;
mod rfc1035;

use std::{
    fs::File,
    io::{BufWriter, Write},
};

pub use config::{config_file, Config, ConfigStream};
use docker::Container;
pub use docker::{DockerState, DockerStateStream};
pub use rfc1035::{RecordData, ResourceRecord};

use crate::rfc1035::{AbsoluteName, Records, RelativeName};

pub fn container_names(container: &Container) -> Vec<RelativeName> {
    let mut names: Vec<RelativeName> = Default::default();

    for name in &container.names {
        names.push(name.trim_start_matches('/').to_lowercase().into());
    }

    if let Some(service) = container.labels.get("com.docker.compose.service") {
        names.push(service.into())
    }

    names
}

pub fn add_network_zones(config: &Config, state: &DockerState, records: &mut Records) {
    for network in state.networks.values() {
        if let Some(zone) = config.network_zone(network) {
            log::trace!("Creating zone {} for network {}", zone, network.name,);

            for container in state.containers.values() {
                if let Some(endpoint) = container.networks.get(&network.id) {
                    if let Some(ip) = endpoint.ip {
                        for name in container_names(container) {
                            records.add_record(zone.prepend(name), RecordData::from(ip));
                        }
                    }
                }
            }
        }
    }
}

pub fn write_zone(config: &Config, state: &DockerState) -> Result<(), String> {
    log::trace!("Writing zone from docker state.");
    let mut records = Records::new();

    add_network_zones(config, state, &mut records);

    let nameserver = AbsoluteName::new("ns.foo.bar");
    for zone in records.zones(&nameserver) {
        let file_name = format!("{}zone", zone.domain);
        let file = File::create(file_name).map_err(|e| format!("Failed to open file: {}", e))?;
        let mut writer = BufWriter::new(file);
        zone.write(&mut writer)
            .map_err(|e| format!("Failed to write zone: {}", e))?;
        writer
            .flush()
            .map_err(|e| format!("Failed to write zone: {}", e))?;
    }

    Ok(())
}
