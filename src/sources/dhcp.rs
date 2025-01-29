use std::path::{Path, PathBuf};

use figment::value::magic::RelativePathBuf;
use reqwest::Client;
use serde::Deserialize;
use tokio::fs::read_to_string;
use tracing::{instrument, Span};

use crate::{
    dns::{Fqdn, RData, Record, RecordSet},
    sources::{RecordStore, SourceConfig, SourceHandle, SourceId, SourceType},
    watcher::{watch, FileEvent, WatchListener},
    Error,
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
            if *name == "*" {
                continue;
            }

            let name = match zone.child(*name) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error=%e, "Error parsing lease file");
                    continue;
                }
            };

            let rdata = match RData::try_from(*ip) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error=%e, "Error parsing lease file");
                    continue;
                }
            };

            records.insert(Record::new(name, rdata));
        }
    }

    records
}

#[instrument(level = "debug", name = "dnsmasq_parse", fields(%source_id, records))]
async fn parse_file(source_id: &SourceId, zone: &Fqdn, lease_file: &Path) -> RecordSet {
    tracing::debug!("Parsing dhcp lease file");

    let data = match read_to_string(lease_file).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read lease file");
            return RecordSet::new();
        }
    };

    let records = parse_dnsmasq(zone, &data);

    let span = Span::current();
    span.record("records", records.len());

    records
}

struct SourceWatcher {
    source_id: SourceId,
    dhcp_config: DhcpConfig,
    lease_file: PathBuf,
    record_store: RecordStore,
}

impl WatchListener for SourceWatcher {
    async fn event(&mut self, _: FileEvent) {
        let records = parse_file(&self.source_id, &self.dhcp_config.zone, &self.lease_file).await;

        self.record_store
            .add_source_records(&self.source_id, records)
            .await
    }
}

impl SourceConfig for DhcpConfig {
    fn source_type() -> SourceType {
        SourceType::Dhcp
    }

    async fn spawn(
        self,
        source_id: SourceId,
        record_store: &RecordStore,
        _: &Client,
    ) -> Result<SourceHandle, Error> {
        let lease_file = self.lease_file.relative();
        let zone = self.zone.clone();

        let watcher = watch(
            &lease_file.clone(),
            SourceWatcher {
                source_id: source_id.clone(),
                record_store: record_store.clone(),
                dhcp_config: self,
                lease_file: lease_file.clone(),
            },
        )
        .await?;

        record_store
            .add_source_records(&source_id, parse_file(&source_id, &zone, &lease_file).await)
            .await;

        Ok(watcher.into())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        str::FromStr,
    };

    use reqwest::Client;
    use tempfile::TempDir;

    use crate::{
        dns::RData,
        sources::{dhcp::DhcpConfig, RecordStore, SourceConfig, SourceId},
        test::{fqdn, name, write_file},
    };

    #[tracing_test::traced_test]
    #[test]
    fn parse_hosts() {
        let zone = fqdn("home.local");

        let records = super::parse_dnsmasq(
            &zone,
            r#"
1646820667 64:4b:c2:7a:cd:83 10.10.1.24 caldigit 01:64:4b:c2:7a:cd:83
1646820649 8c:85:c2:7a:cf:8d 10.10.1.70 laptop 01:8c:85:c2:7a:cf:8d
1646820540 08:aa:0b:47:a3:f8 10.10.1.163 moto-power 01:08:aa:0b:47:a3:f8
1646820689 08:aa:7a:70:15:f6 10.10.1.207 moto-stylus 01:08:aa:7a:70:15:f6
1646820666 f4:d4:ac:db:a5:4c 10.10.1.159 takagi 01:f4:d4:ac:db:a5:4c
bad line
1646820343 f8:0f:01:74:83:c2 10.10.1.240 nest-office *
1646820846 74:d4:8c:85:c2:7a 10.10.15.230 mandelbrot ff:56:50:4d:98:00:02:00:00:ab:11:31:cd:b5:50:8c:85:c2:7a
duid 00:01:00:01:2f:0e:bf:99:00:e2:69:3e:6c:0a
1736266946 1 2b02:c7a:7e12:5b00:1::26b7 caldigit 00:01:00:01:2f:0e:b5:f6:84:2f:57:64:43:9f
1736266909 0 2b02:c7a:7e12:5b00:1::7a36 shashlik 00:01:00:01:2f:0e:b5:f6:84:2f:57:64:43:9f
1736266908 0 2b02:c7a:7e12:5b00:1::36a3 * 00:03:00:01:92:c1:8f:99:66:8c
1736266906 74879383 2a02:c7c:8e12:5b00:1::c8da tikka 00:02:00:00:ab:11:57:4e:b6:bf:29:c2:65:a7
        "#,
        );

        assert_eq!(records.len(), 10);

        assert!(records.contains(
            &fqdn("mandelbrot.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.15.230").unwrap())
        ));

        assert!(records.contains(
            &fqdn("laptop.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.70").unwrap())
        ));

        assert!(records.contains_reverse(
            Ipv4Addr::from_str("10.10.1.70").unwrap(),
            &fqdn("laptop.home.local.")
        ));

        assert!(records.contains_reverse(
            Ipv4Addr::from_str("10.10.15.230").unwrap(),
            &fqdn("mandelbrot.home.local.")
        ));

        assert!(records.contains(
            &fqdn("shashlik.home.local"),
            &RData::Aaaa(Ipv6Addr::from_str("2b02:c7a:7e12:5b00:1::7a36").unwrap())
        ));

        assert!(records.contains(
            &fqdn("caldigit.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.24").unwrap())
        ));

        assert!(records.contains(
            &fqdn("caldigit.home.local"),
            &RData::Aaaa(Ipv6Addr::from_str("2b02:c7a:7e12:5b00:1::26b7").unwrap())
        ));

        assert!(records.contains(
            &fqdn("tikka.home.local"),
            &RData::Aaaa(Ipv6Addr::from_str("2a02:c7c:8e12:5b00:1::c8da").unwrap())
        ));

        assert!(records.contains_reverse(
            Ipv6Addr::from_str("2b02:c7a:7e12:5b00:1::7a36").unwrap(),
            &fqdn("shashlik.home.local.")
        ));
    }

    #[tracing_test::traced_test]
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

        let source_id = SourceId::new(DhcpConfig::source_type(), "test");

        let config = DhcpConfig {
            lease_file: lease_file.as_path().into(),
            zone: fqdn("home.local."),
        };

        let record_store = RecordStore::new();

        let handle = config
            .spawn(source_id.clone(), &record_store, &Client::new())
            .await
            .unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("caldigit.home.local.")))
            .await;

        assert_eq!(records.len(), 2);

        assert!(records.contains(
            &fqdn("caldigit.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.24").unwrap())
        ));

        assert!(records.contains(
            &fqdn("laptop.home.local"),
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

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("other.home.local.")))
            .await;

        assert_eq!(records.len(), 1);

        assert!(!records.has_name(&name("caldigit.home.local.")));

        assert!(!records.has_name(&name("laptop.home.local.")));

        assert!(records.contains(
            &fqdn("other.home.local"),
            &RData::A(Ipv4Addr::from_str("10.10.1.58").unwrap())
        ));

        handle.drop().await;
    }
}
