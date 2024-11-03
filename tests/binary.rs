use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use futures::StreamExt;
use hickory_client::{
    client::AsyncClient,
    op::{DnsResponse, Query, ResponseCode},
    proto::{xfer::DnsRequestOptions, DnsHandle},
    rr::{Name, RecordType},
    udp::UdpClientStream,
};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tempfile::TempDir;
use tokio::{
    fs,
    io::AsyncWriteExt,
    net::UdpSocket,
    process::{Child, Command},
    time::{sleep, timeout},
};
use uuid::{fmt::Simple, Uuid};

async fn write_file<D: AsRef<[u8]>>(path: &Path, data: D) {
    let mut file = fs::File::create(path).await.unwrap();
    file.write_all(data.as_ref()).await.unwrap();
    file.flush().await.unwrap();
}

async fn lookup(
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

    timeout(
        Duration::from_millis(250),
        client.lookup(query, options).next(),
    )
    .await
    .ok()??
    .ok()
}

async fn wait_for_response_inner(address: &str, name: &Name, record_type: RecordType) {
    loop {
        if let Some(response) = lookup(address, name, record_type, true).await {
            if response.response_code() == ResponseCode::NoError {
                return;
            }
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_response(address: &str, name: &Name, record_type: RecordType) {
    let duration = if env::var("KCOV").is_ok() {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(5)
    };

    if timeout(
        duration,
        wait_for_response_inner(address, name, record_type),
    )
    .await
    .is_err()
    {
        panic!("Timed out waiting for response");
    }
}

fn command() -> Command {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_localns"));

    let mut command = if let Ok(kcov) = env::var("KCOV") {
        let mut command = Command::new(kcov);

        let mut buffer = [0_u8; Simple::LENGTH];
        let uuid = Uuid::new_v4().as_simple().encode_lower(&mut buffer);

        let leaf_name = binary.as_path().file_name().unwrap().to_str().unwrap();

        let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("cov")
            .join(format!("{}-{}", leaf_name, uuid));

        command
            .arg("--exclude-pattern=/.cargo")
            .arg("--verify")
            .arg("--output-interval=0")
            .arg(output)
            .arg(env!("CARGO_BIN_EXE_localns"));

        command
    } else {
        Command::new(binary)
    };

    command.env("RUST_LOG", "warn").kill_on_drop(true);
    command
}

async fn kill_server(mut child: Child, pid_file: &Path) {
    let pid_str = fs::read_to_string(pid_file).await.unwrap();
    let pid = Pid::from_raw(pid_str.trim().parse::<i32>().unwrap());

    for signal in [Signal::SIGINT, Signal::SIGQUIT, Signal::SIGKILL] {
        kill(pid, signal).unwrap();

        if timeout(Duration::from_secs(5), child.wait()).await.is_ok() {
            return;
        }
    }

    tracing::warn!("Timed out waiting for child process to finish");
}

#[tracing_test::traced_test]
#[tokio::test]
async fn workingdir() {
    let temp_dir = TempDir::new().unwrap();
    let config_file = temp_dir.path().join("config.yaml");

    write_file(
        &temp_dir.path().join("zone.yml"),
        r#"
test.example.org: 10.10.10.10
"#,
    )
    .await;

    write_file(
        &config_file,
        r#"
pid_file: pid

server:
  port: 53531

sources:
  file:
    zone: zone.yml
"#,
    )
    .await;

    let child = command().current_dir(temp_dir.path()).spawn().unwrap();

    wait_for_response(
        "127.0.0.1:53531",
        &Name::from_utf8("test.example.org.").unwrap(),
        RecordType::A,
    )
    .await;

    kill_server(child, &temp_dir.path().join("pid")).await;
}

#[tracing_test::traced_test]
#[tokio::test]
async fn environment() {
    let temp_dir = TempDir::new().unwrap();
    let config_file = temp_dir.path().join("config.yaml");

    write_file(
        &temp_dir.path().join("zone.yml"),
        r#"
test.example.org: 10.10.10.10
"#,
    )
    .await;

    write_file(
        &config_file,
        r#"
pid_file: pid

server:
  port: 53532

sources:
  file:
    zone: zone.yml
"#,
    )
    .await;

    let child = command()
        .env("LOCALNS_CONFIG", config_file.to_str().unwrap())
        .spawn()
        .unwrap();

    wait_for_response(
        "127.0.0.1:53532",
        &Name::from_utf8("test.example.org.").unwrap(),
        RecordType::A,
    )
    .await;

    kill_server(child, &temp_dir.path().join("pid")).await;
}

#[tracing_test::traced_test]
#[tokio::test]
async fn commandline() {
    let temp_dir = TempDir::new().unwrap();
    let config_file = temp_dir.path().join("config.yaml");

    write_file(
        &temp_dir.path().join("zone.yml"),
        r#"
test.example.org: 10.10.10.10
"#,
    )
    .await;

    write_file(
        &config_file,
        r#"
pid_file: pid

server:
  port: 53533

sources:
  file:
    zone: zone.yml
"#,
    )
    .await;

    let child = command()
        .arg(config_file.to_str().unwrap())
        .spawn()
        .unwrap();

    wait_for_response(
        "127.0.0.1:53533",
        &Name::from_utf8("test.example.org.").unwrap(),
        RecordType::A,
    )
    .await;

    kill_server(child, &temp_dir.path().join("pid")).await;
}
