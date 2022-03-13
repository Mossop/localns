use std::time::Duration;

use futures::future::Abortable;
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;

use crate::{backoff::Backoff, config::deserialize_url, dns::RecordSet};

use super::SourceContext;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct RemoteConfig {
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
        log::error!("Unable to generate API URL: {}", e);
        LoopResult::Quit
    })?;

    match client.get(target).send().await {
        Ok(response) => match response.json::<T>().await {
            Ok(result) => Ok(result),
            Err(e) => {
                log::error!("({}) Failed to parse response from server: {}", name, e);
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            log::error!("({}) Failed to connect to server: {}", name, e);
            Err(LoopResult::Backoff)
        }
    }
}

async fn remote_loop(
    name: &str,
    remote_config: &RemoteConfig,
    client: &Client,
    context: &mut SourceContext,
) -> LoopResult {
    log::trace!(
        "({}) Attempting to connect to remote server at {}...",
        name,
        remote_config.url
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
            log::trace!(
                "({}) Retrieved {} records from the remote server.",
                name,
                records.len()
            );

            context.send(records.clone());
        }

        sleep(Duration::from_secs(30)).await;
    }
}

pub(super) fn source(name: String, remote_config: RemoteConfig, mut context: SourceContext) {
    let registration = context.abort_registration();

    tokio::spawn(Abortable::new(
        async move {
            let mut backoff = Backoff::default();
            let client = Client::new();

            loop {
                match remote_loop(&name, &remote_config, &client, &mut context).await {
                    LoopResult::Backoff => {
                        context.send(RecordSet::new());
                        sleep(backoff.next()).await;
                    }
                    LoopResult::Quit => {
                        return;
                    }
                }
            }
        },
        registration,
    ));
}
