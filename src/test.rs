use std::{
    future::IntoFuture,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    str::FromStr,
    time::Duration,
};

use futures::StreamExt;
use hickory_client::{
    client::AsyncClient,
    op::{DnsResponse, Query, ResponseCode},
    proto::xfer::{DnsHandle, DnsRequestOptions},
    rr::RecordType,
    udp::UdpClientStream,
};
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
    net::UdpSocket,
    time::{self, sleep},
};

use crate::dns::{store::RecordStore, Fqdn, RecordSet};

pub(crate) async fn timeout<F, O>(fut: F) -> O
where
    F: IntoFuture<Output = O>,
{
    match time::timeout(Duration::from_secs(20), fut).await {
        Ok(o) => o,
        Err(_) => panic!("Timed out waiting for expected state"),
    }
}

impl RecordStore {
    pub(crate) fn records(&self) -> RecordSet {
        self.sender.borrow().clone()
    }

    pub(crate) async fn wait_for_records<F>(&self, cb: F) -> RecordSet
    where
        F: FnMut(&RecordSet) -> bool,
    {
        let mut receiver = self.receiver();
        let records = timeout(receiver.wait_for(cb)).await.unwrap().clone();

        #[allow(clippy::let_and_return)]
        records
    }
}

pub(crate) fn name(n: &str) -> Name {
    Name::from_str(n).unwrap()
}

pub(crate) fn fqdn(n: &str) -> Fqdn {
    Fqdn::try_from(n).unwrap()
}

pub(crate) fn rdata_a(ip: &str) -> RData {
    RData::A(rdata::A(Ipv4Addr::from_str(ip).unwrap()))
}

pub(crate) fn rdata_aaaa(ip: &str) -> RData {
    RData::AAAA(rdata::AAAA(Ipv6Addr::from_str(ip).unwrap()))
}

pub(crate) fn rdata_cname(n: &str) -> RData {
    RData::CNAME(rdata::CNAME(name(n)))
}

pub(crate) fn rdata_aname(n: &str) -> RData {
    RData::ANAME(rdata::ANAME(name(n)))
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

pub(crate) async fn traefik_container(config: &str, port: Option<u16>) -> Container {
    let temp_dir = tempdir().unwrap();

    let api_file = temp_dir.path().join("api.yml");
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

    let config_file = temp_dir.path().join("config.yml");
    write_file(&config_file, config).await;

    let wait = HttpWaitStrategy::new("/api/overview")
        .with_port(ContainerPort::Tcp(80))
        .with_header("Host", HeaderValue::from_static("localhost"))
        .with_expected_status_code(200_u16);

    let mut builder = GenericImage::new("localns_test_traefik", "latest")
        .with_wait_for(WaitFor::Http(wait))
        .with_mount(Mount::bind_mount(
            temp_dir.path().to_str().unwrap(),
            "/etc/traefik/conf.d",
        ));

    if let Some(host_port) = port {
        builder = builder.with_mapped_port(host_port, 80.into());
    }

    let container = builder.start().await.unwrap();

    Container {
        _temp_dir: temp_dir,
        container,
    }
}

pub(crate) async fn coredns(data_dir: &Path) -> ContainerAsync<GenericImage> {
    GenericImage::new("localns_test_coredns", "latest")
        .with_wait_for(WaitFor::message_on_stdout("CoreDNS-"))
        .with_mount(Mount::bind_mount(data_dir.to_str().unwrap(), "/data"))
        .start()
        .await
        .unwrap()
}

pub(crate) async fn coredns_container(zone: &str, zonefile: &str) -> Container {
    let temp_dir = tempdir().unwrap();
    let zone_file = temp_dir.path().join("zone");
    let config_file = temp_dir.path().join("Corefile");

    write_file(&config_file, format!("{zone} {{\n  file zone\n}}\n")).await;

    write_file(&zone_file, zonefile).await;

    Container {
        container: coredns(temp_dir.path()).await,
        _temp_dir: temp_dir,
    }
}

pub(crate) async fn lookup(
    address: &str,
    name: &Name,
    record_type: RecordType,
    recurse: bool,
) -> Option<DnsResponse> {
    tracing::trace!("Looking up {record_type} {name} at {address}");
    let stream = UdpClientStream::<UdpSocket>::new(SocketAddr::from_str(address).unwrap());

    let client = AsyncClient::connect(stream);
    let (client, bg) = client.await.unwrap();
    tokio::spawn(bg);

    let query = Query::query(name.clone(), record_type);
    let mut options = DnsRequestOptions::default();
    options.use_edns = true;
    options.recursion_desired = recurse;

    client.lookup(query, options).next().await?.ok()
}

pub(crate) async fn wait_for_response(address: &str, name: &Name, record_type: RecordType) {
    timeout(async {
        loop {
            if let Some(response) = lookup(address, name, record_type, true).await {
                if response.response_code() == ResponseCode::NoError {
                    return;
                }
            }

            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

pub(crate) async fn wait_for_missing_response(address: &str, name: &Name, record_type: RecordType) {
    timeout(async {
        loop {
            if let Some(response) = lookup(address, name, record_type, true).await {
                if response.response_code() == ResponseCode::NXDomain {
                    return;
                }
            }

            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

pub(crate) async fn assert_single_response(
    address: &str,
    name: &Name,
    record_type: RecordType,
    recurse: bool,
    rdata: Option<RData>,
) {
    let response = lookup(address, name, record_type, recurse).await.unwrap();

    if let Some(rdata) = rdata {
        assert_eq!(response.response_code(), ResponseCode::NoError);
        let mut answers = response.answers().to_vec();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let answer = answers.first().unwrap();
        assert_eq!(answer.name(), name);
        assert_eq!(answer.data().unwrap(), &rdata);
    } else {
        assert_eq!(response.response_code(), ResponseCode::NXDomain);
        assert!(response.answers().is_empty());
    }
}

mod integration {
    use std::path::PathBuf;

    use hickory_client::{
        op::{DnsResponse, ResponseCode},
        rr::{self, Name, RecordType},
    };

    use super::*;
    use crate::Server;

    fn assert_records_eq(left: &[rr::Record], right: &[rr::Record]) {
        let mut left = left.to_vec();
        left.sort();
        let mut right = right.to_vec();
        right.sort();
        assert_eq!(left, right);
    }

    fn assert_response_eq(left: DnsResponse, right: DnsResponse) {
        assert_eq!(left.response_code(), right.response_code());
        // assert_eq!(left.authoritative(), right.authoritative());

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
        let left = lookup(left, name, record_type, recurse).await.unwrap();
        let right = lookup(right, name, record_type, recurse).await.unwrap();

        assert_response_eq(left, right);
    }

    async fn compare_servers_all(left: &str, right: &str, name: &Name, record_type: RecordType) {
        compare_servers(left, right, name, record_type, false).await;
        compare_servers(left, right, name, record_type, true).await;
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn coredns_compare() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.yml");

        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_resources")
            .join("coredns_compare");

        write_file(
            &config_file,
            format!(
                r#"
server:
  port: 53531

sources:
  file:
    file1: {}/file1.yml

zones:
  example.org: {{}}
"#,
                test_dir.display()
            ),
        )
        .await;

        let core = coredns(&test_dir).await;
        let core_port = core
            .get_host_port_ipv4(ContainerPort::Udp(53))
            .await
            .unwrap();
        let core_address = format!("127.0.0.1:{core_port}");
        let server = Server::new(&config_file).await.unwrap();
        let localns_address = "127.0.0.1:53531";

        wait_for_response(localns_address, &name("www.example.org."), RecordType::A).await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("www.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("www.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("ipv4.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("ipv4.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("data.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bish.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bish.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bash.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bash.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("aname_1.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("aname_1.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("aname_2.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("aname_2.example.org."),
            RecordType::AAAA,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bad.example.org."),
            RecordType::A,
        )
        .await;

        compare_servers_all(
            localns_address,
            &core_address,
            &name("bad.example.org."),
            RecordType::AAAA,
        )
        .await;

        write_file(
            &config_file,
            format!(
                r#"
server:
  port: 53531

sources:
  file:
    file1: {}/file1.yml
    file2: {}/file2.yml

zones:
  example.org:
    upstream: "[::1]:{}"
"#,
                test_dir.display(),
                test_dir.display(),
                core_port
            ),
        )
        .await;

        wait_for_response(localns_address, &name("bar.example.org."), RecordType::A).await;

        compare_servers(
            localns_address,
            &core_address,
            &name("other.example.org."),
            RecordType::A,
            true,
        )
        .await;

        compare_servers(
            localns_address,
            &core_address,
            &name("foo.example.org."),
            RecordType::A,
            true,
        )
        .await;

        let response = lookup(
            localns_address,
            &name("baz.example.org."),
            RecordType::A,
            true,
        )
        .await
        .unwrap();
        assert_eq!(response.response_code(), ResponseCode::NoError);
        let mut answers = response.answers().to_vec();
        answers.sort();
        assert_eq!(answers.len(), 2);

        let answer = answers.first().unwrap();
        assert_eq!(answer.name(), &name("bar.example.org."));
        assert_eq!(answer.data().unwrap(), &rdata_a("37.23.54.10"));

        let answer = answers.get(1).unwrap();
        assert_eq!(answer.name(), &name("baz.example.org."));
        assert_eq!(answer.data().unwrap(), &rdata_cname("bar.example.org."));

        write_file(
            &config_file,
            format!(
                r#"
server:
  port: 53531

sources:
  file:
    file1: {}/file3.yml

zones:
  example.org:
    upstream: "[::1]:{}"
"#,
                test_dir.display(),
                core_port
            ),
        )
        .await;

        wait_for_response(localns_address, &name("rotty.example.org."), RecordType::A).await;

        compare_servers(
            localns_address,
            &core_address,
            &name("rotty.example.org."),
            RecordType::A,
            true,
        )
        .await;

        server.shutdown().await;
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn http_resolve() {
        let traefik = traefik_container(
            r#"http:
  routers:
    test-router:
      entryPoints:
      - http
      service: test-service
      rule: Host(`test.example.org`)
    api2:
      rule: Host(`traefik.home.local`)
      service: api@internal

  services:
    test-service:
      loadBalancer:
        servers:
        - url: http://foo.bar.com/
"#,
            None,
        )
        .await;
        let traefik_port = traefik.get_tcp_port(80).await;

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.yml");

        write_file(
            &temp_dir.path().join("file1.yml"),
            "traefik.home.local: 127.0.0.1".to_string(),
        )
        .await;

        write_file(
            &config_file,
            format!(
                r#"
server:
  port: 53532

sources:
  file:
    file1: file1.yml
  traefik:
    traefik1:
      url: 'http://traefik.home.local:{traefik_port}/api/'
      interval_ms: 100
"#,
            ),
        )
        .await;

        let server = Server::new(&config_file).await.unwrap();
        let localns_address = "127.0.0.1:53532";

        wait_for_response(localns_address, &name("test.example.org."), RecordType::A).await;

        assert_single_response(
            localns_address,
            &name("test.example.org."),
            RecordType::A,
            true,
            Some(rdata_a("127.0.0.1")),
        )
        .await;

        server.shutdown().await;
    }
}
