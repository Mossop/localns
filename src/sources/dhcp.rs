use std::path::{Path, PathBuf};

use futures::{future::Abortable, StreamExt};
use serde::Deserialize;
use tokio::fs::read_to_string;
use trust_dns_server::resolver::Name;

use crate::{
    config::deserialize_fqdn,
    config::Config,
    record::{rdata, Record, RecordSet},
    watcher::{watch, FileEvent},
};

use super::{create_source, RecordSource};

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct DhcpConfig {
    lease_file: PathBuf,

    #[serde(deserialize_with = "deserialize_fqdn")]
    zone: Name,
}

fn parse_dnsmasq(dhcp_config: &DhcpConfig, data: &str) -> Option<RecordSet> {
    let mut records = RecordSet::new();

    for line in data.lines() {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }

        if let (Some(name), Some(ip)) = (parts.get(3), parts.get(2)) {
            let name = Name::parse(name, Some(&dhcp_config.zone)).unwrap();

            records.insert(Record::new(name, rdata(ip)));
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

pub(super) fn source(name: String, config: Config, dhcp_config: DhcpConfig) -> RecordSource {
    let (sender, registration, source) = create_source();
    let lease_file = config.path(&dhcp_config.lease_file);

    tokio::spawn(Abortable::new(
        async move {
            let records = if lease_file.exists() {
                parse_file(&name, &dhcp_config, &lease_file).await
            } else {
                log::warn!("({}) dhcp file {} is missing.", name, lease_file.display());
                RecordSet::new()
            };

            if sender.send(records).await.is_err() {
                return;
            }

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

                if sender.send(records).await.is_err() {
                    return;
                }
            }
        },
        registration,
    ));

    source
}
