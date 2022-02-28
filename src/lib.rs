mod backoff;
mod config;
mod debounce;
mod rfc1035;
mod sources;

use std::{
    fs::File,
    io::{BufWriter, Write},
    net::Ipv4Addr,
};

pub use config::{config_file, config_stream, Config};
use rfc1035::RecordSet;
pub use rfc1035::{RecordData, ResourceRecord};
pub use sources::RecordSources;

use crate::rfc1035::zones;

// const TRAEFIK_HOST_LABEL: &str = "traefik.hostname";

// pub fn container_names(container: &Container) -> Vec<RelativeName> {
//     let mut names: HashSet<RelativeName> = Default::default();

//     for name in &container.names {
//         names.insert(name.trim_start_matches('/').to_lowercase().into());
//     }

//     if let Some(service) = container.labels.get("com.docker.compose.service") {
//         names.insert(service.to_lowercase().into());
//     }

//     names.drain().collect()
// }

// pub fn add_network_zones(config: &Config, state: &DockerState, records: &mut Records) {
//     for network in state.networks.values() {
//         if let Some(zone) = config.network_zone(network) {
//             log::trace!("Creating zone {} for network {}", zone, network.name,);

//             for container in state.containers.values() {
//                 if let Some(endpoint) = container.networks.get(&network.id) {
//                     if let Some(ip) = endpoint.ip {
//                         for name in container_names(container) {
//                             records.add_record(zone.prepend(name), RecordData::from(ip));
//                         }
//                     }
//                 }
//             }
//         }
//     }
// }

// fn find_traefik(state: &DockerState) -> Option<(&Container, AbsoluteName)> {
//     for container in state.containers.values() {
//         if let Some(zone) = container.labels.get("docker-dns.zone") {
//             return Some((container, zone.into()));
//         }
//     }

//     None
// }

// pub fn add_traefik_zones(config: &Config, state: &DockerState, records: &mut Records) {
//     if let Some((container, name)) = find_traefik(state) {}
// }

pub fn write_zone(_config: &Config, records: RecordSet) -> Result<(), String> {
    log::trace!("Writing zone from records.");

    let nameserver = Ipv4Addr::new(10, 10, 10, 10);
    for zone in zones(records, &nameserver) {
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
