use std::path::{Path, PathBuf};

use figment::value::magic::RelativePathBuf;
use serde::Deserialize;
use tokio::fs::read_to_string;
use tracing::instrument;

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    sources::{SourceConfig, SourceId, SourceType, WatcherHandle},
    watcher::{watch, FileEvent, WatchListener},
    Error, RecordServer, SourceRecords,
};

#[derive(Debug, PartialEq, Deserialize, Clone)]
pub(crate) struct DhcpConfig {
    lease_file: RelativePathBuf,

    zone: Fqdn,
}

fn parse_dnsmasq(zone: &Fqdn, data: &str) -> RecordSet {
    let mut records = RecordSet::new();

    for line in data.lines() {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            continue;
        }

        if let (Some(name), Some(ip)) = (parts.get(3), parts.get(2)) {
            let name = zone.child(name);

            records.insert(Record::new(name, RData::from(*ip)));
        }
    }

    records
}

#[instrument(fields(%source_id), )]
async fn parse_file(source_id: &SourceId, zone: &Fqdn, lease_file: &Path) -> RecordSet {
    tracing::trace!("Parsing dhcp lease file");

    let data = match read_to_string(lease_file).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read lease file");
            return RecordSet::new();
        }
    };

    parse_dnsmasq(zone, &data)
}

struct SourceWatcher<S> {
    source_id: SourceId,
    dhcp_config: DhcpConfig,
    lease_file: PathBuf,
    server: S,
}

impl<S: RecordServer> WatchListener for SourceWatcher<S> {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.source_id, &self.dhcp_config.zone, &self.lease_file).await;

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
    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<WatcherHandle, Error> {
        tracing::trace!("Adding source");
        let lease_file = self.lease_file.relative();

        server
            .add_source_records(SourceRecords::new(
                &source_id,
                None,
                parse_file(&source_id, &self.zone, &lease_file).await,
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

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{
        dns::{Fqdn, RData},
        sources::{dhcp::DhcpConfig, SourceConfig, SourceId, SourceType},
        test::{name, write_file, TestServer},
    };

    #[test]
    fn parse_hosts() {
        let zone = Fqdn::from("home.local");

        let records = super::parse_dnsmasq(
            &zone,
            r#"
1646820667 64:4b:c2:7a:cd:83 10.10.1.24 caldigit 01:64:4b:c2:7a:cd:83
1646820649 8c:85:c2:7a:cf:8d 10.10.1.70 laptop 01:8c:85:c2:7a:cf:8d
1646820540 08:aa:0b:47:a3:f8 10.10.1.163 moto-power 01:08:aa:0b:47:a3:f8
1646820689 08:aa:7a:70:15:f6 10.10.1.207 moto-stylus 01:08:aa:7a:70:15:f6
1646820666 f4:d4:ac:db:a5:4c 10.10.1.159 takagi 01:f4:d4:ac:db:a5:4c
1646820343 f8:0f:01:74:83:c2 10.10.1.240 nest-office *
1646820846 74:d4:8c:85:c2:7a 10.10.15.230 mandelbrot ff:56:50:4d:98:00:02:00:00:ab:11:31:cd:b5:50:8c:85:c2:7a
        "#,
        );

        assert!(records.contains(
            &Fqdn::from("mandelbrot.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.15.230").unwrap())
        ));

        assert!(records.contains(
            &Fqdn::from("laptop.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.70").unwrap())
        ));

        assert!(records.contains_reverse(
            Ipv4Addr::from_str("10.10.1.70").unwrap(),
            &Fqdn::from("laptop.home.local.")
        ));

        assert!(records.contains_reverse(
            Ipv4Addr::from_str("10.10.15.230").unwrap(),
            &Fqdn::from("mandelbrot.home.local.")
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let temp = TempDir::new().unwrap();

        let lease_file = temp.path().join("leases");

        write_file(
            &lease_file,
            r#"
1646820667 64:4b:c2:7a:cd:83 10.10.1.24 caldigit 01:64:4b:c2:7a:cd:83
1646820649 8c:85:c2:7a:cf:8d 10.10.1.70 laptop 01:8c:85:c2:7a:cf:8d
"#,
        )
        .await;

        let source_id = SourceId {
            server_id: Uuid::new_v4(),
            source_type: SourceType::Dhcp,
            source_name: "test".to_string(),
        };

        let config = DhcpConfig {
            lease_file: lease_file.as_path().into(),
            zone: Fqdn::from("home.local."),
        };

        let mut test_server = TestServer::new(&source_id);

        let _handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

        let records = test_server
            .wait_for_records(|records| records.has_name(&name("caldigit.home.local.")))
            .await;

        assert!(records.contains(
            &Fqdn::from("caldigit.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.24").unwrap())
        ));

        assert!(records.contains(
            &Fqdn::from("laptop.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.70").unwrap())
        ));

        assert!(!records.has_name(&name("other.home.local")));

        write_file(
            &lease_file,
            r#"
1646820667 64:4b:c2:7a:cd:83 10.10.1.58 other 01:64:4b:c2:7a:cd:83
        "#,
        )
        .await;

        let records = test_server
            .wait_for_records(|records| records.has_name(&name("other.home.local.")))
            .await;

        assert!(!records.has_name(&name("caldigit.home.local.")));

        assert!(!records.has_name(&name("laptop.home.local.")));

        assert!(records.contains(
            &Fqdn::from("other.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.58").unwrap())
        ));
    }
}
