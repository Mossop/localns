use std::time::Duration;

use futures::future::Abortable;
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::time::sleep;

use crate::{
    backoff::Backoff,
    config::deserialize_url,
    dns::{Fqdn, RData, RDataConfig, Record, RecordSet},
};

use super::SourceContext;

#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
pub struct TraefikConfig {
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
                tracing::error!("({}) Failed to parse response from traefik: {}", name, e);
                Err(LoopResult::Backoff)
            }
        },
        Err(e) => {
            tracing::error!("({}) Failed to connect to traefik: {}", name, e);
            Err(LoopResult::Backoff)
        }
    }
}

fn parse_hosts(rule: &str) -> Result<Vec<Fqdn>, String> {
    let mut hosts: Vec<Fqdn> = Vec::new();

    for item in rule.split("||") {
        hosts.extend(parse_single_host(item.trim())?);
    }

    Ok(hosts)
}

fn parse_single_host(rule: &str) -> Result<Vec<Fqdn>, String> {
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

    if state == State::Post || state == State::Pre {
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
                tracing::warn!("({}) Failed parsing rule for {}: {}", name, r.name, e);
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

async fn traefik_loop(
    name: &str,
    traefik_config: &TraefikConfig,
    client: &Client,
    context: &mut SourceContext,
) -> LoopResult {
    tracing::trace!(
        "({}) Attempting to connect to traefik API at {}...",
        name,
        traefik_config.url
    );

    let version = match api_call::<ApiVersion>(name, client, &traefik_config.url, "version").await {
        Ok(r) => r,
        Err(result) => return result,
    };

    tracing::debug!(
        "({}) Connected to traefik version {}.",
        name,
        version.version
    );

    loop {
        let routers =
            match api_call::<Vec<ApiRouter>>(name, client, &traefik_config.url, "http/routers")
                .await
            {
                Ok(r) => r,
                Err(result) => return result,
            };

        let records = generate_records(name, traefik_config, routers);
        context.send(records);

        sleep(Duration::from_secs(30)).await;
    }
}

pub(super) fn source(name: String, traefik_config: TraefikConfig, mut context: SourceContext) {
    let registration = context.abort_registration();

    tokio::spawn(Abortable::new(
        async move {
            let mut backoff = Backoff::default();
            let client = Client::new();

            loop {
                match traefik_loop(&name, &traefik_config, &client, &mut context).await {
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
