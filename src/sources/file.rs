use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use chrono::Utc;
use figment::value::magic::RelativePathBuf;
use hickory_server::proto::error::ProtoError;
use serde::Deserialize;
use tracing::{instrument, Span};

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    sources::{SourceConfig, SourceHandle, SourceId, SourceType},
    watcher::{watch, FileEvent, WatchListener},
    Error, RecordServer, SourceRecords,
};

pub(crate) type FileConfig = RelativePathBuf;

#[derive(Deserialize, Eq, PartialEq, Debug)]
#[serde(untagged)]
enum RDataItem {
    RData(RData),
    Str(String),
}

impl TryFrom<RDataItem> for RData {
    type Error = ProtoError;

    fn try_from(item: RDataItem) -> Result<Self, Self::Error> {
        match item {
            RDataItem::RData(rdata) => Ok(rdata),
            RDataItem::Str(str) => RData::try_from(str.as_str()),
        }
    }
}

#[derive(Deserialize, Eq, PartialEq, Debug)]
#[serde(untagged)]
enum RDataOneOrMany {
    List(Vec<RDataItem>),
    RData(RDataItem),
}

type ZoneFile = HashMap<Fqdn, RDataOneOrMany>;

#[instrument(level = "debug", name = "zonefile_parse", fields(%source_id, records), err)]
fn parse_file(source_id: &SourceId, zone_file: &Path) -> Result<RecordSet, Error> {
    tracing::debug!("Parsing zone file");

    let f = File::open(zone_file)?;
    let zone_data: ZoneFile = serde_yaml::from_reader(f)?;

    let mut records = RecordSet::new();

    for (name, rdata) in zone_data {
        match rdata {
            RDataOneOrMany::RData(item) => {
                let rdata = match item.try_into() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error=%e, "Error parsing zone file");
                        continue;
                    }
                };
                records.insert(Record::new(name, rdata));
            }
            RDataOneOrMany::List(list) => {
                for item in list {
                    let rdata = match item.try_into() {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error=%e, "Error parsing zone file");
                            continue;
                        }
                    };
                    records.insert(Record::new(name.clone(), rdata));
                }
            }
        }
    }

    let span = Span::current();
    span.record("records", records.len());

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
                self.server
                    .clear_source_records(&self.source_id, Utc::now())
                    .await;
            }
        }
    }
}

impl SourceConfig for FileConfig {
    fn source_type() -> SourceType {
        SourceType::File
    }

    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<SourceHandle<S>, Error> {
        let zone_file = self.relative();

        let watcher = watch(
            &zone_file.clone(),
            SourceWatcher {
                source_id: source_id.clone(),
                server: server.clone(),
                zone_file: zone_file.clone(),
            },
        )
        .await?;

        match parse_file(&source_id, &zone_file) {
            Ok(records) => {
                server
                    .add_source_records(SourceRecords::new(&source_id, None, records))
                    .await
            }
            Err(e) => {
                tracing::warn!(error=%e, "Failed to read zone file");
                server.clear_source_records(&source_id, Utc::now()).await;
            }
        }

        Ok(watcher.into())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        str::FromStr,
    };

    use tempfile::TempDir;
    use tokio::fs;
    use uuid::Uuid;

    use crate::{
        dns::RData,
        sources::{file::FileConfig, SourceConfig, SourceId},
        test::{fqdn, name, write_file, SingleSourceServer},
    };

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let temp = TempDir::new().unwrap();

        let zone_file = temp.path().join("zone.yml");

        write_file(
            &zone_file,
            r#"
www.home.local:
  - 10.14.23.123
  - 1af2:cac:8e12:5b00::2
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

        let handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

        let records = test_server
            .wait_for_records(|records| records.has_name(&name("www.home.local.")))
            .await;

        assert_eq!(records.len(), 3);

        assert!(records.contains(
            &fqdn("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));

        assert!(records.contains(
            &fqdn("www.home.local"),
            &RData::Aaaa(Ipv6Addr::from_str("1af2:cac:8e12:5b00::2").unwrap())
        ));

        assert!(records.contains(
            &fqdn("other.home.local"),
            &RData::Aname(fqdn("www.home.local"))
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
            &fqdn("www.home.local"),
            &RData::A(Ipv4Addr::from_str("10.14.23.123").unwrap())
        ));

        fs::remove_file(&zone_file).await.unwrap();

        let records = test_server.wait_for_maybe_records(|o| o.is_none()).await;
        assert_eq!(records, None);

        handle.drop().await;
    }
}
