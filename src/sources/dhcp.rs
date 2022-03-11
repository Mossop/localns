use std::path::{Path, PathBuf};

use futures::{future::Abortable, StreamExt};
use serde::Deserialize;
use tokio::fs::read_to_string;

use crate::{
    config::Config,
    dns::{Fqdn, RData, Record, RecordSet},
    watcher::{watch, FileEvent},
};

use super::SourceContext;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DhcpConfig {
    lease_file: PathBuf,

    zone: Fqdn,
}

fn parse_dnsmasq(dhcp_config: &DhcpConfig, data: &str) -> Option<RecordSet> {
    let mut records = RecordSet::new();

    for line in data.lines() {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }

        if let (Some(name), Some(ip)) = (parts.get(3), parts.get(2)) {
            let name = dhcp_config.zone.child(name);

            records.insert(Record::new(name, RData::from(*ip)));
        }
    }

    if !records.is_empty() {
        Some(records)
    } else {
        None
    }
}

async fn parse_file(name: &str, dhcp_config: &DhcpConfig, lease_file: &Path) -> RecordSet {
    log::trace!(
        "({}) Parsing dhcp lease file {}...",
        name,
        lease_file.display()
    );

    let data = match read_to_string(lease_file).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("({}) Failed to read lease file: {}", name, e);
            return RecordSet::new();
        }
    };

    if let Some(records) = parse_dnsmasq(dhcp_config, &data) {
        records
    } else {
        RecordSet::new()
    }
}

pub(super) fn source(
    name: String,
    config: Config,
    dhcp_config: DhcpConfig,
    mut context: SourceContext,
) {
    let lease_file = config.path(&dhcp_config.lease_file);

    let registration = context.abort_registration();
    tokio::spawn(Abortable::new(
        async move {
            let records = if lease_file.exists() {
                parse_file(&name, &dhcp_config, &lease_file).await
            } else {
                log::warn!("({}) dhcp file {} is missing.", name, lease_file.display());
                RecordSet::new()
            };

            context.send(records);

            let mut stream = match watch(&lease_file) {
                Ok(stream) => stream,
                Err(e) => {
                    log::error!("({}) {}", name, e);
                    return;
                }
            };

            while let Some(ev) = stream.next().await {
                let records = match ev {
                    FileEvent::Delete => {
                        log::warn!("({}) dhcp file {} is missing.", name, lease_file.display());
                        RecordSet::new()
                    }
                    _ => parse_file(&name, &dhcp_config, &lease_file).await,
                };

                context.send(records);
            }
        },
        registration,
    ));
}
