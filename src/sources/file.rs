use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use figment::value::magic::RelativePathBuf;
use tracing::instrument;

use crate::{
    dns::{Fqdn, RDataConfig, Record, RecordSet},
    sources::{SourceConfig, SourceId, SourceType, WatcherHandle},
    watcher::{watch, FileEvent, WatchListener},
    Error, RecordServer, SourceRecords,
};

pub(crate) type FileConfig = RelativePathBuf;

type LeaseFile = HashMap<Fqdn, RDataConfig>;

#[instrument(fields(%source_id), err)]
fn parse_file(source_id: &SourceId, lease_file: &Path) -> Result<RecordSet, Error> {
    tracing::trace!("Parsing lease file");

    let f = File::open(lease_file)?;
    let leases: LeaseFile = serde_yaml::from_reader(f)?;

    let mut records = RecordSet::new();

    for (name, rdata) in leases {
        records.insert(Record::new(name, rdata.into()));
    }

    Ok(records)
}

struct SourceWatcher<S> {
    source_id: SourceId,
    lease_file: PathBuf,
    server: S,
}

impl<S: RecordServer> WatchListener for SourceWatcher<S> {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.source_id, &self.lease_file).unwrap_or_default();

        self.server
            .add_source_records(SourceRecords::new(&self.source_id, None, records))
            .await;
    }
}

impl SourceConfig for FileConfig {
    type Handle = WatcherHandle;

    fn source_type() -> SourceType {
        SourceType::File
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<WatcherHandle, Error> {
        tracing::trace!("Adding source");
        let lease_file = self.relative();

        server
            .add_source_records(SourceRecords::new(
                &source_id,
                None,
                parse_file(&source_id, &lease_file).unwrap_or_default(),
            ))
            .await;

        let watcher = watch(
            &lease_file.clone(),
            SourceWatcher {
                source_id: source_id.clone(),
                server: server.clone(),
                lease_file,
            },
        )?;

        Ok(WatcherHandle { _watcher: watcher })
    }
}
