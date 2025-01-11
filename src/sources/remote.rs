use std::collections::HashMap;

use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;
use tracing::instrument;

use crate::{
    api::ApiRecords,
    config::deserialize_url,
    run_loop::{Backoff, LoopResult},
    sources::{SourceConfig, SourceId, SourceType, SpawnHandle},
    Error, RecordServer,
};

const POLL_INTERVAL_MS: u64 = 15000;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct RemoteConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
    #[serde(default)]
    interval_ms: Option<u64>,
}

#[instrument(fields(%source_id, %base_url), skip(client))]
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

async fn remote_loop<S: RecordServer>(
    server: S,
    source_id: SourceId,
    remote_config: RemoteConfig,
    client: Client,
) {
    let mut backoff = Backoff::new(remote_config.interval_ms.unwrap_or(POLL_INTERVAL_MS));

    tracing::trace!(
        %source_id,
        url = %remote_config.url,
        "Attempting to connect to remote server",
    );

    let mut seen_sources: HashMap<SourceId, DateTime<Utc>> = HashMap::new();

    loop {
        let api_records =
            match api_call::<ApiRecords>(&source_id, &client, &remote_config.url, "v2/records")
                .await
            {
                Ok(r) => {
                    backoff.reset();
                    r
                }
                Err(e) => {
                    for (source_id, timestamp) in seen_sources.iter() {
                        server.clear_source_records(source_id, *timestamp).await;
                    }

                    match e {
                        LoopResult::Quit => {
                            return;
                        }
                        LoopResult::Sleep => {
                            backoff.reset();
                            sleep(backoff.duration()).await;
                            continue;
                        }
                        LoopResult::Backoff => {
                            backoff.backoff();
                            sleep(backoff.duration()).await;
                            continue;
                        }
                    }
                }
            };

        let mut record_count = 0;
        let old_sources = seen_sources;
        seen_sources = api_records
            .source_records
            .iter()
            .map(|sr| (sr.source_id.clone(), sr.timestamp))
            .collect();

        for (old_source, timestamp) in old_sources {
            if !seen_sources.contains_key(&old_source) {
                server.clear_source_records(&old_source, timestamp).await;
            }
        }

        for source_records in api_records.source_records {
            record_count += source_records.records.len();

            server.add_source_records(source_records).await;
        }

        tracing::trace!(
            %source_id,
            record_count,
            "Retrieved remote records",
        );

        sleep(backoff.duration()).await;
    }
}

impl SourceConfig for RemoteConfig {
    type Handle = SpawnHandle;

    fn source_type() -> SourceType {
        SourceType::Remote
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<SpawnHandle, Error> {
        tracing::trace!("Adding source");

        let handle = {
            let client = Client::new();
            let config = self.clone();

            tokio::spawn(remote_loop(
                server.clone(),
                source_id,
                config.clone(),
                client.clone(),
            ))
        };

        Ok(SpawnHandle { handle })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::{Ipv4Addr, SocketAddr},
        str::FromStr,
        sync::Arc,
    };

    use chrono::Utc;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use crate::{
        api::{ApiConfig, ApiServer},
        config::Config,
        dns::{Fqdn, RData, Record, RecordSet},
        sources::{remote::RemoteConfig, SourceConfig, SourceId, SourceRecords, SourceType},
        test::{fqdn, name, MultiSourceServer},
        ServerId, ServerInner,
    };

    fn build_records<const N: usize>(
        inner: &mut ServerInner,
        records: [(&SourceId, &[(Fqdn, RData)]); N],
    ) {
        inner.records.clear();

        for (source_id, record_list) in records {
            let mut records = RecordSet::default();
            for (fqdn, rdata) in record_list {
                records.insert(Record::new(fqdn.clone(), rdata.clone()));
            }

            let source_records = SourceRecords {
                source_id: source_id.clone(),
                timestamp: Utc::now(),
                records,
            };

            inner.records.insert(source_id.clone(), source_records);
        }
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let local_server = ServerId::new_v4();

        let remote_source_1 = SourceId {
            server_id: ServerId::new_v4(),
            source_type: SourceType::Dhcp,
            source_name: "test1".to_string(),
        };

        let remote_source_2 = SourceId {
            server_id: ServerId::new_v4(),
            source_type: SourceType::File,
            source_name: "test2".to_string(),
        };

        let mut inner = ServerInner {
            config: Config::default(),
            records: HashMap::new(),
        };

        build_records(
            &mut inner,
            [
                (
                    &remote_source_1,
                    &[(
                        fqdn("www.test.local"),
                        RData::A("10.5.23.43".parse().unwrap()),
                    )],
                ),
                (
                    &remote_source_2,
                    &[(
                        fqdn("www.test.local"),
                        RData::A("10.4.2.4".parse().unwrap()),
                    )],
                ),
            ],
        );

        let server_inner = Arc::new(Mutex::new(inner));
        let api_config = ApiConfig {
            address: SocketAddr::new(Ipv4Addr::from_str("0.0.0.0").unwrap().into(), 0),
        };

        let api = ApiServer::new(&api_config, local_server, server_inner.clone()).unwrap();

        let mut test_server = MultiSourceServer::new();

        {
            let source_id = SourceId {
                server_id: Uuid::new_v4(),
                source_type: RemoteConfig::source_type(),
                source_name: "test".to_string(),
            };

            let config = RemoteConfig {
                url: format!("http://localhost:{}/", api.port).parse().unwrap(),
                interval_ms: Some(100),
            };

            let _handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

            let records = test_server
                .wait_for_records(|records| records.has_name(&name("www.test.local.")))
                .await;

            assert_eq!(records.len(), 2);

            let records_1 = records.get(&remote_source_1).unwrap();
            assert_eq!(records_1.len(), 1);
            assert!(records_1.contains(
                &fqdn("www.test.local"),
                &RData::A("10.5.23.43".parse().unwrap())
            ));

            let records_2 = records.get(&remote_source_2).unwrap();
            assert_eq!(records_2.len(), 1);
            assert!(records_2.contains(
                &fqdn("www.test.local"),
                &RData::A("10.4.2.4".parse().unwrap())
            ));

            {
                let mut inner = server_inner.lock().await;
                build_records(
                    &mut inner,
                    [(
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
                );
            }

            let records = test_server
                .wait_for_records(|records| records.has_name(&name("bob.test.local.")))
                .await;

            assert_eq!(records.len(), 1);

            let records_1 = records.get(&remote_source_1).unwrap();
            assert_eq!(records_1.len(), 2);
            assert!(records_1.contains(
                &fqdn("www.test.local"),
                &RData::A("10.5.23.43".parse().unwrap())
            ));
            assert!(records_1.contains(
                &fqdn("bob.test.local"),
                &RData::Aaaa("fe80::1".parse().unwrap())
            ));

            {
                let mut inner = server_inner.lock().await;
                build_records(
                    &mut inner,
                    [(
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
                );
            }

            let records = test_server
                .wait_for_records(|records| records.has_name(&name("done.test.local.")))
                .await;

            assert_eq!(records.len(), 1);

            let records_1 = records.get(&remote_source_1).unwrap();
            assert_eq!(records_1.len(), 2);
            assert!(records_1.contains(
                &fqdn("www.test.local"),
                &RData::A("10.10.2.41".parse().unwrap())
            ));
            assert!(records_1.contains(
                &fqdn("done.test.local"),
                &RData::A("10.1.2.41".parse().unwrap())
            ));
        }

        let records = test_server
            .wait_for_records(|records| !records.has_name(&name("done.test.local.")))
            .await;

        assert!(records.is_empty());

        tracing::trace!("Shutting down");
        api.shutdown().await;
    }
}
