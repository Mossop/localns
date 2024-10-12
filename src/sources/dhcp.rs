use std::path::{Path, PathBuf};

use figment::value::magic::RelativePathBuf;
use serde::Deserialize;
use tokio::fs::read_to_string;
use tracing::instrument;

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    sources::{SourceHandle, WatcherHandle},
    watcher::{watch, FileEvent, WatchListener},
    Error, Server, SourceRecords, UniqueId,
};

#[derive(Debug, PartialEq, Deserialize, Clone)]
pub(crate) struct DhcpConfig {
    lease_file: RelativePathBuf,

    zone: Fqdn,
}

fn parse_dnsmasq(dhcp_config: &DhcpConfig, data: &str) -> RecordSet {
    let mut records = RecordSet::new();

    for line in data.lines() {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return RecordSet::new();
        }

        if let (Some(name), Some(ip)) = (parts.get(3), parts.get(2)) {
            let name = dhcp_config.zone.child(name);

            records.insert(Record::new(name, RData::from(*ip)));
        }
    }

    records
}

#[instrument(fields(source = "dhcp"))]
async fn parse_file(name: &str, dhcp_config: &DhcpConfig, lease_file: &Path) -> RecordSet {
    tracing::trace!("Parsing dhcp lease file");

    let data = match read_to_string(lease_file).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read lease file");
            return RecordSet::new();
        }
    };

    parse_dnsmasq(dhcp_config, &data)
}

struct SourceWatcher {
    source_id: UniqueId,
    name: String,
    dhcp_config: DhcpConfig,
    lease_file: PathBuf,
    server: Server,
}

impl WatchListener for SourceWatcher {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.name, &self.dhcp_config, &self.lease_file).await;

        self.server
            .add_source_records(SourceRecords::new(&self.source_id, None, records))
            .await
    }
}

#[instrument(fields(source = "dhcp"), skip(server))]
pub(super) async fn source(
    name: String,
    server: &Server,
    dhcp_config: &DhcpConfig,
) -> Result<Box<dyn SourceHandle>, Error> {
    tracing::trace!("Adding source");
    let lease_file = dhcp_config.lease_file.relative();
    let source_id = server.id.extend(&lease_file.to_string_lossy());

    server
        .add_source_records(SourceRecords::new(
            &source_id,
            None,
            parse_file(&name, dhcp_config, &lease_file).await,
        ))
        .await;

    let watcher = watch(
        &lease_file.clone(),
        SourceWatcher {
            source_id: source_id.clone(),
            server: server.clone(),
            dhcp_config: dhcp_config.clone(),
            name,
            lease_file,
        },
    )?;

    Ok(Box::new(WatcherHandle {
        source_id,
        _watcher: watcher,
    }))
}
