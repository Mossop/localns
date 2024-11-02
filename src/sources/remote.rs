use std::time::Duration;

use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;
use tracing::instrument;

use crate::{
    backoff::Backoff,
    config::deserialize_url,
    dns::RecordSet,
    sources::{SourceConfig, SourceId, SourceType, SpawnHandle},
    Error, RecordServer, SourceRecords,
};

const POLL_INTERVAL_MS: u64 = 15000;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct RemoteConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
    #[serde(default)]
    interval_ms: Option<u64>,
}

enum LoopResult {
    Backoff,
    Quit,
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
    source_id: &SourceId,
    remote_config: &RemoteConfig,
    client: &Client,
    server: &S,
) -> LoopResult {
    tracing::trace!(
        %source_id,
        url = %remote_config.url,
        "Attempting to connect to remote server",
    );

    let mut records = RecordSet::new();

    loop {
        let new_records =
            match api_call::<RecordSet>(source_id, client, &remote_config.url, "records").await {
                Ok(r) => r,
                Err(result) => return result,
            };

        if new_records != records {
            records = new_records;
            tracing::trace!(
                %source_id,
                records = records.len(),
                "Retrieved remote records",
            );

            server
                .add_source_records(SourceRecords::new(source_id, None, records.clone()))
                .await;
        }

        sleep(Duration::from_millis(
            remote_config.interval_ms.unwrap_or(POLL_INTERVAL_MS),
        ))
        .await;
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
            let source_id = source_id.clone();
            let server = server.clone();
            tokio::spawn(async move {
                let mut backoff = Backoff::default();
                let client = Client::new();

                loop {
                    match remote_loop(&source_id, &self, &client, &server).await {
                        LoopResult::Backoff => {
                            server.clear_source_records(&source_id).await;
                            sleep(backoff.next()).await;
                        }
                        LoopResult::Quit => {
                            return;
                        }
                    }
                }
            })
        };

        Ok(SpawnHandle { handle })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        str::FromStr,
        sync::Arc,
    };

    use tokio::sync::RwLock;
    use uuid::Uuid;

    use crate::{
        api::{ApiConfig, ApiServer},
        dns::{Fqdn, RData, Record, RecordSet},
        sources::{remote::RemoteConfig, SourceConfig, SourceId},
        test::{name, SingleSourceServer},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn integration() {
        let mut api_records = RecordSet::default();
        api_records.insert(Record::new(
            "www.test.local".into(),
            RData::A(Ipv4Addr::from_str("10.5.23.43").unwrap()),
        ));

        let api_records = Arc::new(RwLock::new(api_records));
        let api_config = ApiConfig {
            address: SocketAddr::new(Ipv4Addr::from_str("0.0.0.0").unwrap().into(), 3452),
        };

        let api = ApiServer::new(&api_config, api_records.clone()).unwrap();

        {
            let source_id = SourceId {
                server_id: Uuid::new_v4(),
                source_type: RemoteConfig::source_type(),
                source_name: "test".to_string(),
            };

            let config = RemoteConfig {
                url: "http://localhost:3452/".to_string().parse().unwrap(),
                interval_ms: Some(100),
            };

            let mut test_server = SingleSourceServer::new(&source_id);

            let _handle = config.spawn(source_id.clone(), &test_server).await.unwrap();

            let records = test_server
                .wait_for_records(|records| records.has_name(&name("www.test.local.")))
                .await;

            assert_eq!(records.len(), 1);

            assert!(records.contains(
                &Fqdn::from("www.test.local"),
                &RData::A("10.5.23.43".parse().unwrap())
            ));

            {
                let mut inner = api_records.write().await;
                *inner = RecordSet::default();
            }

            let records = test_server
                .wait_for_records(|records| !records.has_name(&name("www.test.local.")))
                .await;

            assert!(records.is_empty());

            {
                let mut inner = api_records.write().await;
                inner.insert(Record::new(
                    "home.test.local".into(),
                    RData::A(Ipv4Addr::from_str("10.25.23.43").unwrap()),
                ));
                inner.insert(Record::new(
                    "www.test.local".into(),
                    RData::Cname(Fqdn::from("home.test.local")),
                ));
            }

            let records = test_server
                .wait_for_records(|records| records.has_name(&name("www.test.local.")))
                .await;

            assert_eq!(records.len(), 2);

            assert!(records.contains(
                &Fqdn::from("home.test.local"),
                &RData::A("10.25.23.43".parse().unwrap())
            ));

            assert!(records.contains(
                &Fqdn::from("www.test.local"),
                &RData::Cname(Fqdn::from("home.test.local"))
            ));
        }

        api.shutdown().await;
    }
}
