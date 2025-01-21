use std::time::Duration;

use anyhow::bail;
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;
use tracing::{instrument, Span};

use crate::{
    config::deserialize_url,
    dns::{Fqdn, RData, Record, RecordSet},
    run_loop::{LoopResult, RunLoop},
    sources::{RecordStore, SourceConfig, SourceHandle, SourceId, SourceType},
    Error,
};

const POLL_INTERVAL_MS: u64 = 15000;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub(crate) struct TraefikConfig {
    #[serde(deserialize_with = "deserialize_url")]
    url: Url,
    address: Option<RData>,
    #[serde(default)]
    interval_ms: Option<u64>,
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

#[instrument(level = "trace", name = "traefik_api_call", fields(%source_id, %base_url), skip(client))]
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

#[instrument(level = "trace", err)]
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
                match Fqdn::try_from(st.as_str()) {
                    Ok(fqdn) => hosts.push(fqdn),
                    Err(e) => {
                        tracing::warn!(error=%e, hostname = st, "Invalid hostname");
                    }
                }
                State::Post
            }
            (State::Backtick(st), ch) => State::Backtick(format!("{}{}", st, ch)),

            (State::Quote(st), '"') => {
                match Fqdn::try_from(st.as_str()) {
                    Ok(fqdn) => hosts.push(fqdn),
                    Err(e) => {
                        tracing::warn!(error=%e, hostname = st, "Invalid hostname");
                    }
                }
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

fn generate_records(routers: &[ApiRouter]) -> impl Iterator<Item = Fqdn> + '_ {
    routers
        .iter()
        .filter_map(|r| match parse_hosts(&r.rule) {
            Ok(hosts) => Some(hosts),
            Err(e) => {
                tracing::warn!(error = %e, router = r.name, rule = r.rule, "Failed parsing rule");
                None
            }
        })
        .flatten()
}

#[instrument(level = "trace" name = "traefik_fetch_records", fields(%source_id, records), skip(client, traefik_config, target_name, rdata))]
async fn fetch_records(
    source_id: &SourceId,
    client: &Client,
    traefik_config: &TraefikConfig,
    target_name: Option<&Fqdn>,
    rdata: &RData,
) -> Result<RecordSet, LoopResult> {
    let routers =
        api_call::<Vec<ApiRouter>>(source_id, client, &traefik_config.url, "http/routers").await?;

    let records: RecordSet = generate_records(&routers)
        .filter_map(|fqdn| {
            if Some(&fqdn) == target_name {
                None
            } else {
                Some(Record::new(fqdn, rdata.clone()))
            }
        })
        .collect();

    let span = Span::current();
    span.record("records", records.len());

    Ok(records)
}

async fn traefik_loop(
    record_store: RecordStore,
    client: Client,
    source_id: SourceId,
    traefik_config: TraefikConfig,
) -> LoopResult {
    let rdata = if let Some(address) = &traefik_config.address {
        address.clone()
    } else if let Some(host) = traefik_config.url.host_str() {
        match RData::try_from(host) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error=%e, host, "Invalid url");
                return LoopResult::Quit;
            }
        }
    } else {
        return LoopResult::Quit;
    };

    let target_name = match &rdata {
        RData::Cname(name) => Some(name.clone()),
        RData::Aname(name) => Some(name.clone()),
        _ => None,
    };

    tracing::trace!(
        %source_id,
        "Attempting to connect to traefik API",
    );

    let version =
        match api_call::<ApiVersion>(&source_id, &client, &traefik_config.url, "version").await {
            Ok(r) => r,
            Err(result) => return result,
        };

    tracing::debug!(
        %source_id,
        traefik_version = version.version,
        "Connected to traefik",
    );

    loop {
        let records = match fetch_records(
            &source_id,
            &client,
            &traefik_config,
            target_name.as_ref(),
            &rdata,
        )
        .await
        {
            Ok(r) => r,
            Err(result) => return result,
        };

        record_store.add_source_records(&source_id, records).await;

        sleep(Duration::from_millis(
            traefik_config.interval_ms.unwrap_or(POLL_INTERVAL_MS),
        ))
        .await;
    }
}

impl SourceConfig for TraefikConfig {
    fn source_type() -> SourceType {
        SourceType::Traefik
    }

    async fn spawn(
        self,
        source_id: SourceId,
        record_store: &RecordStore,
        client: &Client,
    ) -> Result<SourceHandle, Error> {
        let handle = {
            let backoff = RunLoop::new(self.interval_ms.unwrap_or(POLL_INTERVAL_MS));
            let config = self.clone();
            let client = client.clone();

            tokio::spawn(backoff.run(
                record_store.clone(),
                source_id,
                move |record_store, source_id| {
                    traefik_loop(record_store, client.clone(), source_id, config.clone())
                },
            ))
        };

        Ok(handle.into())
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Client;
    use uuid::Uuid;

    use crate::{
        dns::RData,
        sources::{traefik::TraefikConfig, RecordStore, SourceConfig, SourceId},
        test::{fqdn, name, traefik_container},
    };

    #[tracing_test::traced_test]
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

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn integration() {
        let traefik = traefik_container(
            r#"http:
  routers:
    test-router:
      entryPoints:
      - http
      service: test-service
      rule: Host(`test.example.org`)

  services:
    test-service:
      loadBalancer:
        servers:
        - url: http://foo.bar.com/
"#,
            None,
        )
        .await;
        let port = traefik.get_tcp_port(80).await;

        let source_id = SourceId {
            server_id: Uuid::new_v4(),
            source_type: TraefikConfig::source_type(),
            source_name: "test".to_string(),
        };

        let config = TraefikConfig {
            url: format!("http://localhost:{port}/api/").parse().unwrap(),
            address: None,
            interval_ms: Some(100),
        };

        let record_store = RecordStore::new();

        let handle = config
            .spawn(source_id.clone(), &record_store, &Client::new())
            .await
            .unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("test.example.org.")))
            .await;

        assert_eq!(records.len(), 1);

        assert!(records.contains(&fqdn("test.example.org"), &RData::Aname(fqdn("localhost"))));

        handle.drop().await;

        let config = TraefikConfig {
            url: format!("http://localhost:{port}/api/").parse().unwrap(),
            address: Some(RData::A("10.10.15.23".parse().unwrap())),
            interval_ms: Some(100),
        };

        let record_store = RecordStore::new();

        let handle = config
            .spawn(source_id.clone(), &record_store, &Client::new())
            .await
            .unwrap();

        let records = record_store
            .wait_for_records(|records| records.has_name(&name("test.example.org.")))
            .await;

        assert_eq!(records.len(), 2);

        assert!(records.contains(
            &fqdn("test.example.org"),
            &RData::A("10.10.15.23".parse().unwrap())
        ));

        assert!(records.contains(
            &fqdn("localhost"),
            &RData::A("10.10.15.23".parse().unwrap())
        ));

        drop(traefik);

        record_store
            .wait_for_records(|records| records.is_empty())
            .await;

        handle.drop().await;
    }
}
