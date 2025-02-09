use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::{sync::Mutex, time::sleep};
use tracing::instrument;

use crate::{
    api::ApiRecords,
    config::deserialize_url,
    dns::store::RemoteServerRecords,
    run_loop::{Backoff, LoopResult},
    sources::{RecordStore, SourceConfig, SourceHandle, SourceId, SourceType},
    Error, ServerId,
};

const POLL_INTERVAL_MS: u64 = 15000;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct RemoteConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
    #[serde(default)]
    interval_ms: Option<u64>,
}

#[instrument(level = "trace", name = "remote_api_call", fields(%source_id, %base_url), skip(client))]
async fn api_call<T>(
    source_id: &SourceId,
    client: &Client,
    base_url: &Url,
    method: &str,
) -> Result<T, LoopResult>
where
    T: DeserializeOwned,
{
    let target = base_url.join(method).map_err(|e| {
        tracing::error!("Unable to generate API URL: {}", e);
        LoopResult::Quit
    })?;

    match client.get(target).send().await {
        Ok(response) => match response.json::<T>().await {
            Ok(result) => Ok(result),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse response from server");
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "Failed to connect to server");
            Err(LoopResult::Backoff)
        }
    }
}

#[instrument(level = "trace", name = "remote_fetch_records", skip_all, fields(%source_id, records))]
async fn fetch_records(
    source_id: &SourceId,
    client: &Client,
    remote_config: &RemoteConfig,
    seen_sources: &Arc<Mutex<HashMap<SourceId, DateTime<Utc>>>>,
    record_store: &RecordStore,
    previous_server: &mut Option<ServerId>,
) -> LoopResult {
    let api_records =
        match api_call::<ApiRecords>(source_id, client, &remote_config.url, "v2/records").await {
            Ok(r) => r,
            Err(result) => {
                record_store.clear_source_records(source_id).await;

                seen_sources.lock().await.clear();

                return result;
            }
        };

    if let Some(old_server) = previous_server.replace(api_records.store.server_id) {
        if old_server != api_records.store.server_id {
            tracing::debug!(%source_id,
                url = %remote_config.url,
                server_id = %api_records.store.server_id,
                version = api_records.server_version,
                "Connected to remote server",
            );
        }
    } else {
        tracing::debug!(%source_id,
            url = %remote_config.url,
            server_id = %api_records.store.server_id,
            version = api_records.server_version,
            "Connected to remote server",
        );
    }

    let timestamp = Utc::now();
    let max_expiry = timestamp
        + Duration::milliseconds(
            (remote_config.interval_ms.unwrap_or(POLL_INTERVAL_MS) * 3)
                .try_into()
                .unwrap(),
        );

    let mut remotes = api_records.store.remote;

    for rsr in remotes.values_mut() {
        if rsr.expiry > max_expiry {
            rsr.expiry = max_expiry
        }
    }

    let direct_remote = RemoteServerRecords {
        timestamp,
        expiry: max_expiry,
        records: api_records.store.local,
    };
    remotes.insert(api_records.store.server_id, direct_remote);

    record_store.add_remote_records(remotes).await;

    LoopResult::Sleep
}

async fn remote_loop(
    record_store: RecordStore,
    client: Client,
    source_id: SourceId,
    remote_config: RemoteConfig,
    seen_sources: Arc<Mutex<HashMap<SourceId, DateTime<Utc>>>>,
) {
    let mut backoff = Backoff::new(remote_config.interval_ms.unwrap_or(POLL_INTERVAL_MS));

    loop {
        tracing::trace!(
            %source_id,
            url = %remote_config.url,
            "Attempting to connect to remote server",
        );

        let mut previous_server: Option<ServerId> = None;

        loop {
            match fetch_records(
                &source_id,
                &client,
                &remote_config,
                &seen_sources,
                &record_store,
                &mut previous_server,
            )
            .await
            {
                LoopResult::Quit => {
                    return;
                }
                LoopResult::Sleep => {
                    backoff.reset();
                }
                LoopResult::Backoff => {
                    backoff.backoff();
                    break;
                }
            }

            sleep(backoff.duration()).await;
        }

        sleep(backoff.duration()).await;
    }
}

impl SourceConfig for RemoteConfig {
    fn source_type() -> SourceType {
        SourceType::Remote
    }

    async fn spawn(
        self,
        source_id: SourceId,
        record_store: &RecordStore,
        client: &Client,
    ) -> Result<SourceHandle, Error> {
        let seen_sources = Arc::new(Mutex::new(HashMap::new()));

        let handle = {
            let config = self.clone();

            tokio::spawn(remote_loop(
                record_store.clone(),
                client.clone(),
                source_id,
                config.clone(),
                seen_sources.clone(),
            ))
        };

        Ok(handle.into())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::{Ipv4Addr, SocketAddr},
        path::PathBuf,
        str::FromStr,
        time::Duration,
    };

    use chrono::Utc;
    use hickory_client::rr::RecordType;
    use reqwest::Client;
    use tempfile::TempDir;
    use tokio::time::sleep;

    use crate::{
        api::{ApiConfig, ApiServer},
        dns::{store::RemoteServerRecords, Fqdn, RData, Record, RecordSet},
        sources::{remote::RemoteConfig, RecordStore, SourceConfig, SourceId, SourceType},
        test::{
            assert_single_response, fqdn, name, rdata_a, wait_for_missing_response,
            wait_for_response, write_file,
        },
        Server, ServerId,
    };

    #[allow(clippy::type_complexity)]
    async fn build_records(
        record_store: &RecordStore,
        local_sources: &[(&SourceId, &[(Fqdn, RData)])],
        remote_sources: &[(&ServerId, &SourceId, &[(Fqdn, RData)])],
    ) {
        let mut store_data = record_store.store_data.write().await;
        store_data.local.clear();

        for (source_id, record_list) in local_sources {
            let mut records = RecordSet::default();
            for (fqdn, rdata) in *record_list {
                records.insert(Record::new(fqdn.clone(), rdata.clone()));
            }

            store_data.local.insert((*source_id).clone(), records);
        }

        let mut remote_records: HashMap<ServerId, RemoteServerRecords> = HashMap::new();
        let timestamp = Utc::now();
        let expiry = timestamp + Duration::from_millis(10000);

        for (server_id, source_id, record_list) in remote_sources {
            let mut records = RecordSet::default();
            for (fqdn, rdata) in *record_list {
                records.insert(Record::new(fqdn.clone(), rdata.clone()));
            }

            let mut server_records = HashMap::new();
            server_records.insert((*source_id).clone(), records);

            remote_records.insert(
                **server_id,
                RemoteServerRecords {
                    timestamp,
                    expiry,
                    records: server_records,
                },
            );
        }

        store_data.remote = remote_records;
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let remote_source_1 = SourceId::new(SourceType::Dhcp, "test1");

        let remote_server_2 = ServerId::new_v4();
        let remote_source_2 = SourceId::new(SourceType::File, "test2");

        let api_record_store = RecordStore::new();

        build_records(
            &api_record_store,
            &[(
                &remote_source_1,
                &[(
                    fqdn("www.test.local"),
                    RData::A("10.5.23.43".parse().unwrap()),
                )],
            )],
            &[(
                &remote_server_2,
                &remote_source_2,
                &[
                    (
                        fqdn("www.test.local"),
                        RData::A("10.4.2.4".parse().unwrap()),
                    ),
                    (
                        fqdn("lost.test.local"),
                        RData::A("10.4.20.4".parse().unwrap()),
                    ),
                ],
            )],
        )
        .await;

        let api_config = ApiConfig {
            address: SocketAddr::new(Ipv4Addr::from_str("0.0.0.0").unwrap().into(), 0),
        };

        let api = ApiServer::new(&api_config, api_record_store.clone()).unwrap();

        let record_store = RecordStore::new();

        let source_id = SourceId::new(RemoteConfig::source_type(), "test");

        let config = RemoteConfig {
            url: format!("http://localhost:{}/", api.port).parse().unwrap(),
            interval_ms: Some(100),
        };

        let handle = config
            .spawn(source_id.clone(), &record_store, &Client::new())
            .await
            .unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("www.test.local.")))
            .await;

        assert_eq!(records.len(), 3);

        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::A("10.5.23.43".parse().unwrap())
        ));

        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::A("10.4.2.4".parse().unwrap())
        ));

        assert!(records.contains(
            &fqdn("lost.test.local"),
            &RData::A("10.4.20.4".parse().unwrap())
        ));

        build_records(
            &api_record_store,
            &[(
                &remote_source_1,
                &[
                    (
                        fqdn("www.test.local"),
                        RData::A("10.5.23.43".parse().unwrap()),
                    ),
                    (
                        fqdn("bob.test.local"),
                        RData::Aaaa("fe80::1".parse().unwrap()),
                    ),
                ],
            )],
            &[],
        )
        .await;

        let records = record_store
            .wait_for_records(|records| !records.has_name(&name("lost.test.local.")))
            .await;

        assert_eq!(records.len(), 2);

        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::A("10.5.23.43".parse().unwrap())
        ));
        assert!(records.contains(
            &fqdn("bob.test.local"),
            &RData::Aaaa("fe80::1".parse().unwrap())
        ));

        build_records(
            &api_record_store,
            &[(
                &remote_source_1,
                &[
                    (
                        fqdn("www.test.local"),
                        RData::A("10.10.2.41".parse().unwrap()),
                    ),
                    (
                        fqdn("done.test.local"),
                        RData::A("10.1.2.41".parse().unwrap()),
                    ),
                ],
            )],
            &[],
        )
        .await;

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("done.test.local.")))
            .await;

        assert_eq!(records.len(), 2);

        assert!(records.contains(
            &fqdn("www.test.local"),
            &RData::A("10.10.2.41".parse().unwrap())
        ));
        assert!(records.contains(
            &fqdn("done.test.local"),
            &RData::A("10.1.2.41".parse().unwrap())
        ));

        handle.drop().await;

        api.shutdown().await;
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn broken_remotes() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.yml");

        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_resources")
            .join("broken_remotes");

        write_file(
            &config_file,
            format!(
                r#"
server:
  port: 53533

sources:
  file:
    file1: {}/file1.yml
  remote:
    remote1:
      url: http://remote.home.local:8032/
      interval_ms: 100
"#,
                test_dir.display()
            ),
        )
        .await;

        let server = Server::new(&config_file).await.unwrap();
        let localns_address = "127.0.0.1:53533";

        wait_for_response(localns_address, &name("test.home.local."), RecordType::A).await;

        sleep(Duration::from_secs(1)).await;

        assert_single_response(
            localns_address,
            &name("test.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.10.10.10")),
        )
        .await;

        assert_single_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
            true,
            None,
        )
        .await;

        let record_store = RecordStore::new();

        let remote_server = ServerId::new_v4();
        let remote_source = SourceId::new(SourceType::Dhcp, "test1");

        build_records(
            &record_store,
            &[],
            &[(
                &remote_server,
                &remote_source,
                &[(
                    fqdn("provided.home.local"),
                    RData::A("10.5.23.43".parse().unwrap()),
                )],
            )],
        )
        .await;

        let api_config = ApiConfig {
            address: SocketAddr::new(Ipv4Addr::from_str("0.0.0.0").unwrap().into(), 8032),
        };

        let api = ApiServer::new(&api_config, record_store.clone()).unwrap();

        wait_for_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
        )
        .await;

        assert_single_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.5.23.43")),
        )
        .await;

        assert_single_response(
            localns_address,
            &name("test.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.10.10.10")),
        )
        .await;

        api.shutdown().await;

        wait_for_missing_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
        )
        .await;

        assert_single_response(
            localns_address,
            &name("test.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.10.10.10")),
        )
        .await;

        let api = ApiServer::new(&api_config, record_store.clone()).unwrap();

        wait_for_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
        )
        .await;

        assert_single_response(
            localns_address,
            &name("test.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.10.10.10")),
        )
        .await;

        assert_single_response(
            localns_address,
            &name("provided.home.local."),
            RecordType::A,
            true,
            Some(rdata_a("10.5.23.43")),
        )
        .await;

        api.shutdown().await;

        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_cycle() {
        let temp_dir = TempDir::new().unwrap();
        let config_file_a = temp_dir.path().join("config_a.yml");
        let config_file_b1 = temp_dir.path().join("config_b1.yml");
        let config_file_b2 = temp_dir.path().join("config_b2.yml");

        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_resources")
            .join("remote_cycle");

        write_file(
            &config_file_a,
            format!(
                r#"
server:
  port: 5365

api:
  address: 127.0.0.1:8065

sources:
  file:
    file1: {}/server_a.yml
  remote:
    remote1:
      url: http://127.0.0.1:8066/
      interval_ms: 300
"#,
                test_dir.display()
            ),
        )
        .await;

        let server_a = Server::new(&config_file_a).await.unwrap();
        let server_a_address = "127.0.0.1:5365";

        write_file(
            &config_file_b1,
            format!(
                r#"
server:
  port: 5366

api:
  address: 127.0.0.1:8066

sources:
  file:
    file1: {}/server_b1.yml
  remote:
    remote1:
      url: http://127.0.0.1:8065/
      interval_ms: 100
"#,
                test_dir.display()
            ),
        )
        .await;

        let server_b = Server::new(&config_file_b1).await.unwrap();
        let server_b_address = "127.0.0.1:5366";

        // Both servers should contain both records.
        wait_for_response(server_a_address, &name("host.servera.com."), RecordType::A).await;
        wait_for_response(server_a_address, &name("host.serverb.com."), RecordType::A).await;
        wait_for_response(server_b_address, &name("host.servera.com."), RecordType::A).await;
        wait_for_response(server_b_address, &name("host.serverb.com."), RecordType::A).await;

        write_file(
            &config_file_b2,
            format!(
                r#"
server:
  port: 5366

api:
  address: 127.0.0.1:8066

sources:
  file:
    file1: {}/server_b2.yml
  remote:
    remote1:
      url: http://127.0.0.1:8065/
      interval_ms: 100
"#,
                test_dir.display()
            ),
        )
        .await;

        // "Restarting" server b gives it a new server ID. It will immediately retrieve records
        // from server a which includes the old records from server b with the old server ID.
        server_b.shutdown().await;

        let server_b = Server::new(&config_file_b2).await.unwrap();
        wait_for_response(server_b_address, &name("host.servera.com."), RecordType::A).await;
        wait_for_response(server_b_address, &name("host.serverb.com."), RecordType::A).await;

        // Server b should eventually expire its old record.
        wait_for_missing_response(server_b_address, &name("old.serverb.com."), RecordType::A).await;

        // Server a should also lose the old server b record.
        wait_for_missing_response(server_a_address, &name("old.serverb.com."), RecordType::A).await;

        server_b.shutdown().await;

        // Server a should lose the server b record.
        wait_for_missing_response(server_a_address, &name("host.serverb.com."), RecordType::A)
            .await;

        // But should retain its own record.
        assert_single_response(
            server_a_address,
            &name("host.servera.com."),
            RecordType::A,
            true,
            Some(rdata_a("10.10.10.10")),
        )
        .await;

        server_a.shutdown().await;
    }
}
