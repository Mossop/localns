use std::time::Duration;

use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;
use tracing::instrument;

use crate::{
    backoff::Backoff,
    config::deserialize_url,
    dns::RecordSet,
    sources::{SourceHandle, SpawnHandle},
    Error, Server, SourceRecords, UniqueId,
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

async fn api_call<T>(
    name: &str,
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
                tracing::error!("({}) Failed to parse response from server: {}", name, e);
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            tracing::error!("({}) Failed to connect to server: {}", name, e);
            Err(LoopResult::Backoff)
        }
    }
}

async fn remote_loop(
    name: &str,
    source_id: &UniqueId,
    remote_config: &RemoteConfig,
    client: &Client,
    server: &Server,
) -> LoopResult {
    tracing::trace!(
        source = "remote",
        name,
        url = %remote_config.url,
        "Attempting to connect to remote server",
    );

    let mut records = RecordSet::new();

    loop {
        let new_records =
            match api_call::<RecordSet>(name, client, &remote_config.url, "records").await {
                Ok(r) => r,
                Err(result) => return result,
            };

        if new_records != records {
            records = new_records;
            tracing::trace!(
                source = "remote",
                name,
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

#[instrument(fields(source = "remote"), skip(server))]
pub(super) async fn source(
    name: String,
    server: &Server,
    remote_config: &RemoteConfig,
) -> Result<Box<dyn SourceHandle>, Error> {
    tracing::trace!("Adding source");
    let source_id = server.id.extend(remote_config.url.as_ref());

    let handle = {
        let source_id = source_id.clone();
        let remote_config = remote_config.clone();
        let server = server.clone();
        tokio::spawn(async move {
            let mut backoff = Backoff::default();
            let client = Client::new();

            loop {
                match remote_loop(&name, &source_id, &remote_config, &client, &server).await {
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

    Ok(Box::new(SpawnHandle { source_id, handle }))
}
