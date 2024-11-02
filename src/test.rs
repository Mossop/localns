use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    path::Path,
    str::FromStr,
    time::Duration,
};

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
pub(crate) struct MultiSourceServer {
    sender: watch::Sender<HashMap<SourceId, RecordSet>>,
    receiver: watch::Receiver<HashMap<SourceId, RecordSet>>,
}

impl MultiSourceServer {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = watch::channel(HashMap::new());

        Self { sender, receiver }
    }

    pub(crate) async fn wait_for_change(&mut self) -> HashMap<SourceId, RecordSet> {
        match timeout(Duration::from_secs(20), self.receiver.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => panic!("Record stream closed"),
            Err(_) => panic!("Timed out waiting for new records"),
        }

        self.receiver.borrow_and_update().clone()
    }

    pub(crate) async fn wait_for_state<F>(&mut self, mut cb: F) -> HashMap<SourceId, RecordSet>
    where
        F: FnMut(&HashMap<SourceId, RecordSet>) -> bool,
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
                return records.clone();
            }
        }
    }

    pub(crate) async fn wait_for_records<F>(&mut self, mut cb: F) -> HashMap<SourceId, RecordSet>
    where
        F: FnMut(&RecordSet) -> bool,
    {
        self.wait_for_state(|map| {
            for record_set in map.values() {
                if cb(record_set) {
                    return true;
                }
            }

            false
        })
        .await
    }
}

#[derive(Clone)]
pub(crate) struct SingleSourceServer {
    source_id: SourceId,
    inner: MultiSourceServer,
}

impl SingleSourceServer {
    pub(crate) fn new(source_id: &SourceId) -> Self {
        Self {
            source_id: source_id.clone(),
            inner: MultiSourceServer::new(),
        }
    }

    pub(crate) async fn wait_for_maybe_records<F>(&mut self, mut cb: F) -> Option<RecordSet>
    where
        F: FnMut(Option<&RecordSet>) -> bool,
    {
        self.inner
            .wait_for_state(|map| cb(map.get(&self.source_id)))
            .await
            .get(&self.source_id)
            .cloned()
    }

    pub(crate) async fn wait_for_records<F>(&mut self, cb: F) -> RecordSet
    where
        F: FnMut(&RecordSet) -> bool,
    {
        self.inner
            .wait_for_records(cb)
            .await
            .get(&self.source_id)
            .cloned()
            .unwrap()
    }
}

impl RecordServer for SingleSourceServer {
    async fn add_source_records(&self, new_records: SourceRecords) {
        assert_eq!(new_records.source_id, self.source_id);
        self.inner.add_source_records(new_records).await;
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        assert_eq!(source_id, &self.source_id);
        self.inner.clear_source_records(source_id).await;
    }

    async fn prune_sources(&self, keep: &HashSet<SourceId>) {
        self.inner.prune_sources(keep).await;
    }
}

impl RecordServer for MultiSourceServer {
    async fn add_source_records(&self, new_records: SourceRecords) {
        let mut new_set = self.receiver.borrow().clone();
        new_set.insert(new_records.source_id, new_records.records);
        self.sender.send(new_set).unwrap();
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        let mut new_set = self.receiver.borrow().clone();
        new_set.remove(source_id);
        self.sender.send(new_set).unwrap();
    }

    async fn prune_sources(&self, keep: &HashSet<SourceId>) {
        let mut new_set = self.receiver.borrow().clone();
        for old_key in new_set
            .keys()
            .cloned()
            .collect::<HashSet<SourceId>>()
            .difference(keep)
        {
            new_set.remove(old_key);
        }
        self.sender.send(new_set).unwrap();
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

pub(crate) async fn coredns(data_dir: &Path) -> ContainerAsync<GenericImage> {
    GenericImage::new("coredns/coredns", "1.11.3")
        .with_wait_for(WaitFor::message_on_stdout("CoreDNS-"))
        .with_cmd(["-conf", "/data/Corefile"])
        .with_mount(Mount::bind_mount(data_dir.to_str().unwrap(), "/data"))
        .start()
        .await
        .unwrap()
}

pub(crate) async fn coredns_container(zone: &str, zonefile: &str) -> Container {
    let temp_dir = tempdir().unwrap();
    let zone_file = temp_dir.path().join("zone");
    let config_file = temp_dir.path().join("Corefile");

    write_file(&config_file, format!("{zone} {{\n  file /data/zone\n}}\n")).await;

    write_file(&zone_file, zonefile).await;

    Container {
        container: coredns(temp_dir.path()).await,
        _temp_dir: temp_dir,
    }
}

mod integration {
    use std::{net::SocketAddr, path::PathBuf};

    use futures::StreamExt;
    use hickory_client::{
        client::AsyncClient,
        op::{DnsResponse, Query},
        proto::xfer::{DnsHandle, DnsRequestOptions},
        rr::{self, Name, RecordType},
        udp::UdpClientStream,
    };
    use tokio::net::UdpSocket;

    use super::*;
    use crate::Server;

    async fn lookup(
        address: &str,
        name: &Name,
        record_type: RecordType,
        recurse: bool,
    ) -> DnsResponse {
        tracing::trace!("Looking up {record_type} {name} at {address}");
        let stream = UdpClientStream::<UdpSocket>::new(SocketAddr::from_str(address).unwrap());

        let client = AsyncClient::connect(stream);
        let (client, bg) = client.await.unwrap();
        tokio::spawn(bg);

        let query = Query::query(name.clone(), record_type);
        let mut options = DnsRequestOptions::default();
        options.recursion_desired = recurse;

        client.lookup(query, options).next().await.unwrap().unwrap()
    }

    fn assert_records_eq(left: &[rr::Record], right: &[rr::Record]) {
        let mut left = left.to_vec();
        left.sort();
        let mut right = right.to_vec();
        right.sort();
        assert_eq!(left.len(), right.len());

        for (left, right) in left.into_iter().zip(right.into_iter()) {
            assert_eq!(left, right);
        }
    }

    fn assert_response_eq(left: DnsResponse, right: DnsResponse) {
        assert_eq!(left.response_code(), right.response_code());
        assert_eq!(left.authoritative(), right.authoritative());

        assert_records_eq(left.answers(), right.answers());
        assert_records_eq(left.additionals(), right.additionals());
    }

    async fn compare_servers(
        left: &str,
        right: &str,
        name: &Name,
        record_type: RecordType,
        recurse: bool,
    ) {
        let left = lookup(left, name, record_type, recurse).await;
        let right = lookup(right, name, record_type, recurse).await;

        assert_response_eq(left, right);
    }

    #[tokio::test]
    async fn test1() {
        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_resources")
            .join("test1");
        let config_file = test_dir.join("config.yml");

        let core = coredns(&test_dir).await;
        let core_address = format!(
            "127.0.0.1:{}",
            core.get_host_port_ipv4(ContainerPort::Udp(53))
                .await
                .unwrap()
        );
        let server = Server::new(&config_file).await.unwrap();
        let localns_address = "127.0.0.1:53531";

        compare_servers(
            localns_address,
            &core_address,
            &name("www.example.org."),
            RecordType::A,
            true,
        )
        .await;

        compare_servers(
            localns_address,
            &core_address,
            &name("data.example.org."),
            RecordType::A,
            true,
        )
        .await;

        server.shutdown().await;
    }
}
