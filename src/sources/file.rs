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

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

    use tempfile::TempDir;
    use tokio::{fs, io::AsyncWriteExt};
    use uuid::Uuid;

    use crate::{
        dns::{Fqdn, RData},
        sources::{file::FileConfig, SourceConfig, SourceId, SourceType},
        test::{name, TestServer},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let temp = TempDir::new().unwrap();

        let zone_file = temp.path().join("zone.yml");

        fs::File::create(&zone_file)
            .await
            .unwrap()
            .write_all(
                r#"
www.home.local: 10.14.23.123
other.home.local: www.home.local
"#
                .as_bytes(),
            )
            .await
            .unwrap();

        let source_id = SourceId {
            server_id: Uuid::new_v4(),
            source_type: SourceType::File,
            source_name: "test".to_string(),
        };

        let config = FileConfig::from(zone_file.as_path());

        let test_server = TestServer::new(&source_id);

        let _handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

        let records = test_server
            .wait_for_records(|records| records.has_name(&name("www.home.local.")))
            .await;

        assert!(records.contains(
            &Fqdn::from("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));

        assert!(records.contains(
            &Fqdn::from("other.home.local"),
            &RData::Cname(Fqdn::from("www.home.local"))
        ));

        fs::File::create(&zone_file)
            .await
            .unwrap()
            .write_all(
                r#"
www.home.local: 10.14.23.123
"#
                .as_bytes(),
            )
            .await
            .unwrap();

        let records = test_server
            .wait_for_records(|records| !records.has_name(&name("other.home.local.")))
            .await;

        assert!(!records.has_name(&name("other.home.local")));

        assert!(records.contains(
            &Fqdn::from("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));
    }
}
