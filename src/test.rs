use std::{net::Ipv4Addr, path::Path, str::FromStr, time::Duration};

use hickory_server::proto::rr::{domain::Name, rdata, RData};
use reqwest::header::HeaderValue;
use tempfile::{tempdir, TempDir};
use testcontainers::{
    core::{wait::HttpWaitStrategy, ContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tokio::{fs, io::AsyncWriteExt, sync::watch, time::timeout};

use crate::{
    dns::RecordSet,
    sources::{SourceId, SourceRecords},
    RecordServer,
};

#[derive(Clone)]
pub(crate) struct TestServer {
    source_id: SourceId,
    sender: watch::Sender<Option<RecordSet>>,
    receiver: watch::Receiver<Option<RecordSet>>,
}

impl TestServer {
    pub(crate) fn new(source_id: &SourceId) -> Self {
        let (sender, receiver) = watch::channel(None);

        Self {
            source_id: source_id.clone(),
            sender,
            receiver,
        }
    }

    async fn wait_for_change(&mut self) -> Option<RecordSet> {
        match timeout(Duration::from_secs(10), self.receiver.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => panic!("Record stream closed"),
            Err(_) => panic!("Timed out waiting for new records"),
        }

        self.receiver.borrow_and_update().clone()
    }

    pub(crate) async fn wait_for_maybe_records<F>(&mut self, mut cb: F) -> Option<RecordSet>
    where
        F: FnMut(&Option<RecordSet>) -> bool,
    {
        {
            let records = self.receiver.borrow_and_update();
            if cb(&records) {
                return records.clone();
            }
        }

        loop {
            let records = self.wait_for_change().await;
            if cb(&records) {
                return records;
            }
        }
    }

    pub(crate) async fn wait_for_records<F>(&mut self, mut cb: F) -> RecordSet
    where
        F: FnMut(&RecordSet) -> bool,
    {
        self.wait_for_maybe_records(|maybe| {
            if let Some(records) = maybe {
                cb(records)
            } else {
                false
            }
        })
        .await
        .unwrap()
    }
}

impl RecordServer for TestServer {
    async fn add_source_records(&self, new_records: SourceRecords) {
        assert_eq!(new_records.source_id, self.source_id);

        self.sender.send(Some(new_records.records)).unwrap();
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        assert_eq!(source_id, &self.source_id);

        self.sender.send(None).unwrap();
    }
}

pub(crate) fn name(n: &str) -> Name {
    Name::from_str(n).unwrap()
}

pub(crate) fn rdata_a(ip: &str) -> RData {
    RData::A(rdata::A(Ipv4Addr::from_str(ip).unwrap()))
}

pub(crate) fn rdata_cname(n: &str) -> RData {
    RData::CNAME(rdata::CNAME(name(n)))
}

pub(crate) struct Container {
    _temp_dir: TempDir,
    container: ContainerAsync<GenericImage>,
}

impl Container {
    pub(crate) async fn get_udp_port(&self, port: u16) -> u16 {
        self.container
            .get_host_port_ipv4(ContainerPort::Udp(port))
            .await
            .unwrap()
    }

    pub(crate) async fn get_tcp_port(&self, port: u16) -> u16 {
        self.container
            .get_host_port_ipv4(ContainerPort::Tcp(port))
            .await
            .unwrap()
    }
}

pub(crate) async fn write_file<D: AsRef<[u8]>>(path: &Path, data: D) {
    let mut file = fs::File::create(path).await.unwrap();
    file.write_all(data.as_ref()).await.unwrap();
    file.flush().await.unwrap();
}

pub(crate) async fn traefik_container(config: &str) -> Container {
    let temp_dir = tempdir().unwrap();

    let config_file = temp_dir.path().join("traefik.yml");

    write_file(
        &config_file,
        r#"
api: {}

entryPoints:
  http:
    address: ":80"

providers:
  file:
    directory: /etc/traefik/conf.d
"#,
    )
    .await;

    let provider_dir = temp_dir.path().join("conf.d");
    fs::create_dir(&provider_dir).await.unwrap();

    let api_file = provider_dir.join("api.yml");
    write_file(
        &api_file,
        r#"
http:
  routers:
    api:
      rule: Host(`localhost`)
      service: api@internal
"#,
    )
    .await;

    let config_file = provider_dir.join("config.yml");
    write_file(&config_file, config).await;

    let wait = HttpWaitStrategy::new("/api/overview")
        .with_port(ContainerPort::Tcp(80))
        .with_header("Host", HeaderValue::from_static("localhost"))
        .with_expected_status_code(200_u16);

    let container = GenericImage::new("traefik", "v3.1")
        .with_wait_for(WaitFor::Http(wait))
        .with_mount(Mount::bind_mount(
            temp_dir.path().to_str().unwrap(),
            "/etc/traefik",
        ))
        .start()
        .await
        .unwrap();

    Container {
        _temp_dir: temp_dir,
        container,
    }
}

pub(crate) async fn coredns_container(zone: &str, zonefile: &str) -> Container {
    let temp_dir = tempdir().unwrap();
    let zone_file = temp_dir.path().join("zone");
    let config_file = temp_dir.path().join("Corefile");

    write_file(&config_file, format!("{zone} {{\n  file /data/zone\n}}\n")).await;

    write_file(&zone_file, zonefile).await;

    let container = GenericImage::new("coredns/coredns", "1.11.3")
        .with_wait_for(WaitFor::message_on_stdout("CoreDNS-"))
        .with_cmd(["-conf", "/data/Corefile"])
        .with_mount(Mount::bind_mount(
            temp_dir.path().to_str().unwrap(),
            "/data",
        ))
        .start()
        .await
        .unwrap();

    Container {
        _temp_dir: temp_dir,
        container,
    }
}
