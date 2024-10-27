use std::{net::Ipv4Addr, ops::Deref, str::FromStr, sync::Arc, time::Duration};

use hickory_server::proto::rr::{domain::Name, rdata, RData};
use reqwest::header::HeaderValue;
use tempfile::{tempdir, TempDir};
use testcontainers::{
    core::{wait::HttpWaitStrategy, ContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Mutex, Notify},
    time::timeout,
};

use crate::{
    dns::RecordSet,
    sources::{SourceId, SourceRecords},
    RecordServer,
};

#[derive(Clone)]
pub(crate) struct TestServer {
    source_id: SourceId,
    notify: Arc<Notify>,
    records: Arc<Mutex<Option<RecordSet>>>,
}

impl TestServer {
    pub(crate) fn new(source_id: &SourceId) -> Self {
        Self {
            source_id: source_id.clone(),
            notify: Arc::new(Notify::new()),
            records: Default::default(),
        }
    }

    pub(crate) async fn records(&self) -> Option<RecordSet> {
        self.records.lock().await.clone()
    }

    pub(crate) async fn wait_for_change(&self) {
        if timeout(Duration::from_secs(1), self.notify.notified())
            .await
            .is_err()
        {
            panic!("Timed out waiting for new records");
        }
    }

    pub(crate) async fn wait_for_records<F>(&self, mut cb: F) -> RecordSet
    where
        F: FnMut(&RecordSet) -> bool,
    {
        loop {
            let notified = {
                let inner = self.records.lock().await;
                if let Some(records) = inner.deref() {
                    if cb(records) {
                        return records.clone();
                    }
                }

                self.wait_for_change()
            };

            notified.await;
        }
    }
}

impl RecordServer for TestServer {
    async fn add_source_records(&self, new_records: SourceRecords) {
        assert_eq!(new_records.source_id, self.source_id);

        let mut inner = self.records.lock().await;
        self.notify.notify_waiters();
        inner.replace(new_records.records);
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        assert_eq!(source_id, &self.source_id);

        let mut inner = self.records.lock().await;
        self.notify.notify_waiters();
        inner.take();
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

pub(crate) async fn traefik_container(config: &str) -> Container {
    let temp_dir = tempdir().unwrap();

    let config_file = temp_dir.path().join("traefik.yml");

    let mut file = fs::File::create(&config_file).await.unwrap();
    file.write_all(
        r#"api: {}

entryPoints:
  http:
    address: ":80"

providers:
  file:
    directory: /etc/traefik/conf.d
"#
        .as_bytes(),
    )
    .await
    .unwrap();

    let provider_dir = temp_dir.path().join("conf.d");
    fs::create_dir(&provider_dir).await.unwrap();

    let api_file = provider_dir.join("api.yml");
    let mut file = fs::File::create(&api_file).await.unwrap();
    file.write_all(
        r#"http:
  routers:
    api:
      rule: Host(`localhost`)
      service: api@internal
"#
        .as_bytes(),
    )
    .await
    .unwrap();

    let config_file = provider_dir.join("config.yml");
    let mut file = fs::File::create(&config_file).await.unwrap();
    file.write_all(config.as_bytes()).await.unwrap();

    println!("Starting traefik container...");

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

    let mut file = fs::File::create(&config_file).await.unwrap();
    file.write_all(format!("{zone} {{\n  file /data/zone\n}}\n").as_bytes())
        .await
        .unwrap();

    let mut file = fs::File::create(&zone_file).await.unwrap();
    file.write_all(zonefile.as_bytes()).await.unwrap();

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
