use std::{net::Ipv4Addr, ops::Deref, str::FromStr, sync::Arc, time::Duration};

use hickory_server::proto::rr::{domain::Name, rdata, RData};
use tempfile::{tempdir, TempDir};
use testcontainers::{
    core::{ContainerPort, Mount},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt},
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

    println!("Starting coredns container...");

    let container = GenericImage::new("coredns/coredns", "1.11.3")
        .with_cmd(["-conf", "/data/Corefile"])
        .with_mount(Mount::bind_mount(
            temp_dir.path().to_str().unwrap(),
            "/data",
        ))
        .pull_image()
        .await
        .unwrap()
        .start()
        .await
        .unwrap();

    println!(
        "Got host port: {}",
        container.get_host_port_ipv4(53).await.unwrap()
    );

    let mut stdout = container.stdout(true);
    let mut stderr = container.stderr(true);
    let mut out_line = String::new();
    let mut err_line = String::new();
    loop {
        tokio::select! {
            r = stdout.read_line(&mut out_line) => {
                r.unwrap();
                eprint!("out: {out_line}");
                if out_line.contains("CoreDNS-") {
                    break;
                }
                out_line.clear();
            }
            r = stderr.read_line(&mut err_line) => {
                r.unwrap();
                eprint!("err: {err_line}");
                err_line.clear();
            }
        };
    }

    Container {
        _temp_dir: temp_dir,
        container,
    }
}
