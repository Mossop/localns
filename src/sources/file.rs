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
fn parse_file(source_id: &SourceId, zone_file: &Path) -> Result<RecordSet, Error> {
    tracing::trace!("Parsing zone file");

    let f = File::open(zone_file)?;
    let leases: LeaseFile = serde_yaml::from_reader(f)?;

    let mut records = RecordSet::new();

    for (name, rdata) in leases {
        records.insert(Record::new(name, rdata.into()));
    }

    Ok(records)
}

struct SourceWatcher<S> {
    source_id: SourceId,
    zone_file: PathBuf,
    server: S,
}

impl<S: RecordServer> WatchListener for SourceWatcher<S> {
    async fn event(&mut self, _: FileEvent) {
        match parse_file(&self.source_id, &self.zone_file) {
            Ok(records) => {
                self.server
                    .add_source_records(SourceRecords::new(&self.source_id, None, records))
                    .await
            }
            Err(e) => {
                tracing::warn!(error=%e, "Failed to read zone file");
                self.server.clear_source_records(&self.source_id).await;
            }
        }
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
        let zone_file = self.relative();

        match parse_file(&source_id, &zone_file) {
            Ok(records) => {
                server
                    .add_source_records(SourceRecords::new(&source_id, None, records))
                    .await
            }
            Err(e) => {
                tracing::warn!(error=%e, "Failed to read zone file");
                server.clear_source_records(&source_id).await;
            }
        }

        let watcher = watch(
            &zone_file.clone(),
            SourceWatcher {
                source_id: source_id.clone(),
                server: server.clone(),
                zone_file,
            },
        )?;

        Ok(WatcherHandle { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

    use tempfile::TempDir;
    use tokio::fs;
    use uuid::Uuid;

    use crate::{
        dns::{Fqdn, RData},
        sources::{file::FileConfig, SourceConfig, SourceId},
        test::{name, write_file, SingleSourceServer},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let temp = TempDir::new().unwrap();

        let zone_file = temp.path().join("zone.yml");

        write_file(
            &zone_file,
            r#"
www.home.local: 10.14.23.123
other.home.local: www.home.local
"#,
        )
        .await;

        let source_id = SourceId {
            server_id: Uuid::new_v4(),
            source_type: FileConfig::source_type(),
            source_name: "test".to_string(),
        };

        let config = FileConfig::from(zone_file.as_path());

        let mut test_server = SingleSourceServer::new(&source_id);

        let _handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

        let records = test_server
            .wait_for_records(|records| records.has_name(&name("www.home.local.")))
            .await;

        assert_eq!(records.len(), 2);

        assert!(records.contains(
            &Fqdn::from("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));

        assert!(records.contains(
            &Fqdn::from("other.home.local"),
            &RData::Cname(Fqdn::from("www.home.local"))
        ));

        write_file(
            &zone_file,
            r#"
www.home.local: 10.14.23.123
"#,
        )
        .await;

        let records = test_server
            .wait_for_records(|records| !records.has_name(&name("other.home.local.")))
            .await;

        assert_eq!(records.len(), 1);

        assert!(!records.has_name(&name("other.home.local")));

        assert!(records.contains(
            &Fqdn::from("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));

        fs::remove_file(&zone_file).await.unwrap();

        let records = test_server.wait_for_maybe_records(|o| o.is_none()).await;
        assert_eq!(records, None);
    }
}
