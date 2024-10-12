use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use figment::value::magic::RelativePathBuf;
use tracing::instrument;

use crate::{
    dns::{Fqdn, RDataConfig, Record, RecordSet},
    sources::{SourceHandle, WatcherHandle},
    watcher::{watch, FileEvent, WatchListener},
    Error, Server, SourceRecords, UniqueId,
};

pub(crate) type FileConfig = RelativePathBuf;

type LeaseFile = HashMap<Fqdn, RDataConfig>;

#[instrument(fields(source = "file"), err)]
fn parse_file(name: &str, lease_file: &Path) -> Result<RecordSet, Error> {
    tracing::trace!("Parsing lease file");

    let f = File::open(lease_file)?;
    let leases: LeaseFile = serde_yaml::from_reader(f)?;

    let mut records = RecordSet::new();

    for (name, rdata) in leases {
        records.insert(Record::new(name, rdata.into()));
    }

    Ok(records)
}

struct SourceWatcher {
    source_id: UniqueId,
    name: String,
    lease_file: PathBuf,
    server: Server,
}

impl WatchListener for SourceWatcher {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.name, &self.lease_file).unwrap_or_default();

        self.server
            .add_source_records(SourceRecords::new(&self.source_id, None, records))
            .await;
    }
}

#[instrument(fields(source = "file"), skip(server))]
pub(super) async fn source(
    name: String,
    server: &Server,
    file_config: &FileConfig,
) -> Result<Box<dyn SourceHandle>, Error> {
    tracing::trace!("Adding source");
    let lease_file = file_config.relative();
    let source_id = server.id.extend(&lease_file.to_string_lossy());

    server
        .add_source_records(SourceRecords::new(
            &source_id,
            None,
            parse_file(&name, &lease_file).unwrap_or_default(),
        ))
        .await;

    let watcher = watch(
        &lease_file.clone(),
        SourceWatcher {
            source_id: source_id.clone(),
            server: server.clone(),
            name,
            lease_file,
        },
    )?;

    Ok(Box::new(WatcherHandle {
        source_id,
        _watcher: watcher,
    }))
}
