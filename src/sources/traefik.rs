use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::Abortable;
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    backoff::Backoff,
    config::Config,
    record::{fqdn, rdata, Name, Record, RecordSet},
};

use super::{create_source, RecordSource};

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct TraefikConfig {
    address: String,

    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ApiRouter {
    name: String,
    rule: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ApiVersion {
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Codename")]
    _code_name: String,
    #[serde(rename = "startDate")]
    _start_date: DateTime<Utc>,
}

enum LoopResult {
    Backoff,
    Quit,
}

async fn api_call<T>(
    name: &str,
    client: &Client,
    base_url: &str,
    method: &str,
) -> Result<T, LoopResult>
where
    T: DeserializeOwned,
{
    match client.get(format!("{}{}", base_url, method)).send().await {
        Ok(response) => match response.json::<T>().await {
            Ok(result) => Ok(result),
            Err(e) => {
                log::error!("({}) Failed to parse response from traefik: {}", name, e);
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            log::error!("({}) Failed to connect to traefik: {}", name, e);
            Err(LoopResult::Backoff)
        }
    }
}

fn parse_hosts(rule: &str) -> Result<Vec<Name>, String> {
    #[derive(Debug, PartialEq, Eq)]
    enum State {
        Pre,
        Backtick(String),
        Quote(String),
        EscapedQuote(String),
        Post,
    }

    let mut hosts = Vec::new();
    if !rule.starts_with("Host(") || !rule.ends_with(')') {
        return Ok(hosts);
    }

    let mut state = State::Pre;

    for char in rule[5..rule.len() - 1].chars() {
        state = match (state, char) {
            (State::Pre, ' ' | '\t') => State::Pre,
            (State::Pre, '`') => State::Backtick("".into()),
            (State::Pre, '"') => State::Quote("".into()),
            (State::Pre, ch) => {
                return Err(format!(
                    "Unexpected character '{}' when expecting a string",
                    ch
                ))
            }

            (State::Backtick(st), '`') => {
                hosts.push(fqdn(&st));
                State::Post
            }
            (State::Backtick(st), ch) => State::Backtick(format!("{}{}", st, ch)),

            (State::Quote(st), '"') => {
                hosts.push(fqdn(&st));
                State::Post
            }
            (State::Quote(st), '\\') => State::EscapedQuote(st),
            (State::Quote(st), ch) => State::Quote(format!("{}{}", st, ch)),

            (State::EscapedQuote(st), '"') => State::Quote(format!("{}\"", st)),
            (State::EscapedQuote(_), ch) => {
                return Err(format!(
                    "Unexpected character '{}' when a control character",
                    ch
                ))
            }

            (State::Post, ' ' | '\t') => State::Post,
            (State::Post, ',') => State::Pre,
            (State::Post, ch) => {
                return Err(format!(
                    "Unexpected character '{}' when expecting a comma or the end of the rule",
                    ch
                ))
            }
        }
    }

    if state == State::Post {
        Ok(hosts)
    } else {
        Err(format!("Unexpected end of rule (in state {:?})", state))
    }
}

fn generate_records(
    name: &str,
    traefik_config: &TraefikConfig,
    routers: Vec<ApiRouter>,
) -> RecordSet {
    routers
        .iter()
        .filter_map(|r| match parse_hosts(&r.rule) {
            Ok(hosts) => Some(hosts),
            Err(e) => {
                log::warn!("({}) Failed parsing rule for {}: {}", name, r.name, e);
                None
            }
        })
        .flatten()
        .map(|name| Record::new(name, rdata(&traefik_config.address)))
        .collect()
}

async fn traefik_loop(
    name: &str,
    _config: &Config,
    traefik_config: &TraefikConfig,
    client: &Client,
    sender: &mpsc::Sender<RecordSet>,
) -> LoopResult {
    let base_url = match traefik_config.url {
        Some(ref url) => {
            if url.ends_with('/') {
                url.clone()
            } else {
                format!("{}/", url)
            }
        }
        None => format!("http://{}/api/", traefik_config.address),
    };

    log::trace!(
        "({}) Attempting to connect to traefik API at {}...",
        name,
        base_url
    );

    let version = match api_call::<ApiVersion>(name, client, &base_url, "version").await {
        Ok(r) => r,
        Err(result) => return result,
    };

    log::info!(
        "({}) Connected to traefik version {}.",
        name,
        version.version
    );

    loop {
        let routers =
            match api_call::<Vec<ApiRouter>>(name, client, &base_url, "http/routers").await {
                Ok(r) => r,
                Err(result) => return result,
            };

        let records = generate_records(name, traefik_config, routers);
        if sender.send(records).await.is_err() {
            return LoopResult::Quit;
        }

        sleep(Duration::from_secs(30)).await;
    }
}

pub(super) fn source(name: String, config: Config, traefik_config: TraefikConfig) -> RecordSource {
    let (sender, registration, source) = create_source();

    tokio::spawn(Abortable::new(
        async move {
            let client = Client::new();
            let mut backoff = Backoff::default();

            loop {
                match traefik_loop(&name, &config, &traefik_config, &client, &sender).await {
                    LoopResult::Backoff => {
                        if sender.send(RecordSet::new()).await.is_err() {
                            return;
                        }

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

    source
}
