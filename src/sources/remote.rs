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
    Error, Server, SourceRecords,
};

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct RemoteConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
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

async fn remote_loop(
    source_id: &SourceId,
    remote_config: &RemoteConfig,
    client: &Client,
    server: &Server,
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

        sleep(Duration::from_secs(30)).await;
    }
}

impl SourceConfig for RemoteConfig {
    type Handle = SpawnHandle;

    fn source_type() -> SourceType {
        SourceType::Remote
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn(self, source_id: SourceId, server: &Server) -> Result<SpawnHandle, Error> {
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
