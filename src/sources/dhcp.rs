use std::path::{Path, PathBuf};

use figment::value::magic::RelativePathBuf;
use serde::Deserialize;
use tokio::fs::read_to_string;
use tracing::instrument;

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    sources::{SourceConfig, SourceId, SourceType, WatcherHandle},
    watcher::{watch, FileEvent, WatchListener},
    Error, Server, SourceRecords,
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

#[instrument(fields(%source_id), )]
async fn parse_file(
    source_id: &SourceId,
    dhcp_config: &DhcpConfig,
    lease_file: &Path,
) -> RecordSet {
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
    source_id: SourceId,
    dhcp_config: DhcpConfig,
    lease_file: PathBuf,
    server: Server,
}

impl WatchListener for SourceWatcher {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.source_id, &self.dhcp_config, &self.lease_file).await;

        self.server
            .add_source_records(SourceRecords::new(&self.source_id, None, records))
            .await
    }
}

impl SourceConfig for DhcpConfig {
    type Handle = WatcherHandle;

    fn source_type() -> SourceType {
        SourceType::Dhcp
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn(self, source_id: SourceId, server: &Server) -> Result<WatcherHandle, Error> {
        tracing::trace!("Adding source");
        let lease_file = self.lease_file.relative();

        server
            .add_source_records(SourceRecords::new(
                &source_id,
                None,
                parse_file(&source_id, &self, &lease_file).await,
            ))
            .await;

        let watcher = watch(
            &lease_file.clone(),
            SourceWatcher {
                source_id,
                server: server.clone(),
                dhcp_config: self,
                lease_file,
            },
        )?;

        Ok(WatcherHandle { _watcher: watcher })
    }
}
