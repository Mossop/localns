use std::time::Duration;

use anyhow::bail;
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;
use tracing::instrument;

use crate::{
    backoff::Backoff,
    config::deserialize_url,
    dns::{Fqdn, RData, RDataConfig, Record, RecordSet},
    sources::{SourceConfig, SourceId, SourceType, SpawnHandle},
    Error, RecordServer, SourceRecords,
};

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct TraefikConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
    address: Option<RDataConfig>,
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
}

enum LoopResult {
    Backoff,
    Quit,
}

#[instrument(fields(%source_id), skip(client))]
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
        tracing::error!(error = %e, "Unable to generate API URL");
        LoopResult::Quit
    })?;

    match client.get(target).send().await {
        Ok(response) => match response.json::<T>().await {
            Ok(result) => Ok(result),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse response from traefik");
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "Failed to connect to traefik");
            Err(LoopResult::Backoff)
        }
    }
}

fn parse_hosts(rule: &str) -> Result<Vec<Fqdn>, Error> {
    let mut hosts: Vec<Fqdn> = Vec::new();

    for item in rule.split("||") {
        hosts.extend(parse_single_host(item.trim())?);
    }

    Ok(hosts)
}

#[instrument]
fn parse_single_host(rule: &str) -> Result<Vec<Fqdn>, Error> {
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
                bail!("Unexpected character '{}' when expecting a string", ch);
            }

            (State::Backtick(st), '`') => {
                hosts.push(st.into());
                State::Post
            }
            (State::Backtick(st), ch) => State::Backtick(format!("{}{}", st, ch)),

            (State::Quote(st), '"') => {
                hosts.push(st.into());
                State::Post
            }
            (State::Quote(st), '\\') => State::EscapedQuote(st),
            (State::Quote(st), ch) => State::Quote(format!("{}{}", st, ch)),

            (State::EscapedQuote(st), '"') => State::Quote(format!("{}\"", st)),
            (State::EscapedQuote(_), ch) => {
                bail!("Unexpected character '{}' when a control character", ch);
            }

            (State::Post, ' ' | '\t') => State::Post,
            (State::Post, ',') => State::Pre,
            (State::Post, ch) => {
                bail!(
                    "Unexpected character '{}' when expecting a comma or the end of the rule",
                    ch
                );
            }
        }
    }

    if state == State::Post || state == State::Pre {
        Ok(hosts)
    } else {
        bail!("Unexpected end of rule (in state {:?})", state);
    }
}

#[instrument(fields(%source_id), skip(routers, traefik_config))]
fn generate_records(
    source_id: &SourceId,
    traefik_config: &TraefikConfig,
    routers: Vec<ApiRouter>,
) -> RecordSet {
    let rdata = if let Some(address) = &traefik_config.address {
        address.clone().into()
    } else if let Some(host) = traefik_config.url.host_str() {
        host.into()
    } else {
        return RecordSet::new();
    };

    let mut names: Vec<Fqdn> = routers
        .iter()
        .filter_map(|r| match parse_hosts(&r.rule) {
            Ok(hosts) => Some(hosts),
            Err(e) => {
                tracing::warn!(error = %e, router = r.name, rule = r.rule, "Failed parsing rule");
                None
            }
        })
        .flatten()
        .collect();

    if let RData::Cname(ref name) = rdata {
        names = names.drain(..).filter(|n| n != name).collect();
    }

    names
        .drain(..)
        .map(|name| Record::new(name, rdata.clone()))
        .collect()
}

async fn traefik_loop<S: RecordServer>(
    source_id: &SourceId,
    traefik_config: &TraefikConfig,
    client: &Client,
    server: &S,
) -> LoopResult {
    tracing::trace!(
        %source_id,
        "Attempting to connect to traefik API",
    );

    let version =
        match api_call::<ApiVersion>(source_id, client, &traefik_config.url, "version").await {
            Ok(r) => r,
            Err(result) => return result,
        };

    tracing::debug!(
        %source_id,
        traefik_version = version.version,
        "Connected to traefik",
    );

    loop {
        let routers = match api_call::<Vec<ApiRouter>>(
            source_id,
            client,
            &traefik_config.url,
            "http/routers",
        )
        .await
        {
            Ok(r) => r,
            Err(result) => return result,
        };

        let records = generate_records(source_id, traefik_config, routers);
        server
            .add_source_records(SourceRecords::new(source_id, None, records))
            .await;

        sleep(Duration::from_secs(30)).await;
    }
}

impl SourceConfig for TraefikConfig {
    type Handle = SpawnHandle;

    fn source_type() -> SourceType {
        SourceType::Traefik
    }

    #[instrument(fields(%source_id), skip(self, server))]
    async fn spawn<S: RecordServer>(
        self,
        source_id: SourceId,
        server: &S,
    ) -> Result<SpawnHandle, Error> {
        let handle = {
            let source_id = source_id.clone();
            let server = server.clone();
            tokio::spawn(async move {
                let mut backoff = Backoff::default();
                let client = Client::new();

                loop {
                    match traefik_loop(&source_id, &self, &client, &server).await {
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
    #[test]
    fn parse_hosts() {
        fn do_parse(rule: &str) -> Vec<String> {
            super::parse_hosts(rule)
                .expect("Should be no parse error")
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<String>>()
        }

        assert_eq!(
            do_parse("Host(`allthethings.dev`)"),
            vec!["allthethings.dev."]
        );

        assert_eq!(
            do_parse("Host(   `allthethings.dev`  )"),
            vec!["allthethings.dev."]
        );

        assert_eq!(
            do_parse("Host(   \"allthethings.dev\")"),
            vec!["allthethings.dev."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`,`foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`, `foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev` , `foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`, `foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse(
                "Host(`phpmyadmin.cloud.oxymoronical.com`,`postfixadmin.cloud.oxymoronical.com`,)"
            ),
            vec![
                "phpmyadmin.cloud.oxymoronical.com.",
                "postfixadmin.cloud.oxymoronical.com."
            ]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`)||Host(`foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`) ||Host(`foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`)|| Host(`foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );

        assert_eq!(
            do_parse("Host(`allthethings.dev`) || Host(`foo.example.com`)"),
            vec!["allthethings.dev.", "foo.example.com."]
        );
    }
}
