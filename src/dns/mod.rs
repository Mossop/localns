use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::Error;
use futures::FutureExt;
use hickory_server::{
    proto::{
        op::{Query, ResponseCode},
        rr::{self, Name, RecordType},
    },
    ServerFuture,
};
use serde::Deserialize;
use tokio::{
    join,
    net::{TcpListener, UdpSocket},
    sync::{watch::Receiver, RwLock},
};
use tracing::{instrument, Span};

mod handler;
mod query;
mod record;
pub(crate) mod store;
mod upstream;

pub(crate) use record::{Fqdn, RData, Record, RecordSet};
pub(crate) use upstream::Upstream;

use self::handler::Handler;
use crate::{
    config::{ZoneConfigProvider, Zones},
    dns::query::QueryState,
};

#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ServerConfig {
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerState<Z> {
    pub(crate) receiver: Receiver<RecordSet>,
    pub(crate) zones: Arc<RwLock<Z>>,
}

async fn resolve_name<Z: ZoneConfigProvider + Clone>(
    server_state: ServerState<Z>,
    name: String,
) -> Result<
    Box<dyn Iterator<Item = SocketAddr> + Send + 'static>,
    Box<dyn std::error::Error + Send + Sync + 'static>,
> {
    let locked = server_state.locked().await;
    let items = locked.resolve_http_address(name).await.unwrap_or_default();
    Ok(Box::new(items.into_iter()))
}

impl<Z: ZoneConfigProvider + Clone + Send + Sync + 'static> reqwest::dns::Resolve
    for ServerState<Z>
{
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        resolve_name(self.clone(), name.as_str().to_string()).boxed()
    }
}

pub(crate) struct LockedServerState<Z> {
    pub(crate) records: RecordSet,
    pub(crate) zones: Z,
}

impl<Z: Clone> ServerState<Z> {
    pub(crate) fn new(receiver: Receiver<RecordSet>, zones: Z) -> Self {
        Self {
            receiver,
            zones: Arc::new(RwLock::new(zones)),
        }
    }

    pub(crate) async fn replace_zones(&self, zones: Z) {
        let mut locked = self.zones.write().await;
        *locked = zones;
    }

    pub(crate) async fn locked(&self) -> LockedServerState<Z> {
        let zones = self.zones.read().await.clone();
        let records = self.receiver.borrow().clone();

        LockedServerState { zones, records }
    }
}

impl<Z: ZoneConfigProvider> LockedServerState<Z> {
    #[instrument(level = "trace", skip(self))]
    async fn resolve_http_address(&self, name: String) -> Result<Vec<SocketAddr>, Error> {
        let mut name = Name::from_str(&name)?;
        name.set_fqdn(true);

        let mut results = Vec::<SocketAddr>::new();

        let mut ipv4_query_state = QueryState::new(Query::query(name.clone(), RecordType::A), true);
        let mut ipv6_query_state =
            QueryState::new(Query::query(name.clone(), RecordType::AAAA), true);

        join!(
            self.perform_query(&mut ipv4_query_state),
            self.perform_query(&mut ipv6_query_state)
        );

        results.extend(
            ipv4_query_state
                .resolve_name(&name)
                .filter_map(|rdata| match rdata {
                    rr::RData::A(a) => Some(SocketAddr::new(a.0.into(), 0)),
                    _ => None,
                }),
        );

        results.extend(
            ipv6_query_state
                .resolve_name(&name)
                .filter_map(|rdata| match rdata {
                    rr::RData::AAAA(aaaa) => Some(SocketAddr::new(aaaa.0.into(), 0)),
                    _ => None,
                }),
        );

        Ok(results)
    }

    #[instrument(level = "trace", fields(%name), skip(self, query_state))]
    async fn resolve_name(&self, name: &Name, query_state: &mut QueryState) {
        let fqdn = Fqdn::from(name.clone());
        let config = self.zones.zone_config(&fqdn);

        let mut needs_recursion = true;
        let query_type = query_state.query_type();

        let records: Vec<rr::Record> = self
            .records
            .lookup(name, query_state.query_class(), query_type)
            .filter_map(|record| {
                needs_recursion = false;

                // If we have a final record then the name exists.
                let record_type = record.rdata().record_type();
                if record_type == query_type
                    || matches!(record_type, RecordType::A | RecordType::AAAA)
                {
                    query_state.response_code = ResponseCode::NoError;
                }

                if !record.rdata().matches(query_type) {
                    return None;
                }

                match record.rdata() {
                    RData::Aname(fqdn) if query_state.query_type() != RecordType::ANAME => {
                        if !query_state.resolve_aliases {
                            query_state.aliases.insert(name.clone(), fqdn.name());
                            None
                        } else {
                            // Add this record as a CNAME to allow name resolution
                            // to work. This will also cause an unknown to be added
                            // for lookup.
                            Some(rr::Record::from_rdata(
                                name.clone(),
                                record.ttl.unwrap_or(config.ttl),
                                rr::RData::CNAME(rr::rdata::CNAME(fqdn.name())),
                            ))
                        }
                    }
                    _ => record.raw(&config),
                }
            })
            .collect();

        if !config.upstreams.is_empty() && name == query_state.query.name() {
            query_state.recursion_available = true;
        }

        if !records.is_empty() {
            query_state.add_answers(records);

            if name == query_state.query.name() {
                query_state.soa = config.soa();
            }
        };

        if needs_recursion && query_state.recursion_desired {
            for upstream in &config.upstreams {
                upstream.resolve(name, query_state).await;
            }
        }
    }

    async fn lookup_names(&self, query_state: &mut QueryState) {
        while let Some(name) = query_state.next_unknown() {
            self.resolve_name(&name, query_state).await;
        }
    }

    #[instrument(level = "trace", fields(
        name = %query_state.query.name(),
        qtype = query_state.query.query_type().to_string(),
        class = query_state.query.query_class().to_string(),
        response_code,
    ), skip(self, query_state))]
    pub(crate) async fn perform_query(&self, query_state: &mut QueryState) {
        self.lookup_names(query_state).await;

        if !query_state.aliases.is_empty() {
            let mut alias_query_state = query_state.for_aliases();
            self.lookup_names(&mut alias_query_state).await;

            if query_state.response_code == ResponseCode::NXDomain {
                query_state.response_code = alias_query_state.response_code;
            }

            let mut records = Vec::new();

            for (name, alias) in query_state.aliases.iter() {
                let fqdn = Fqdn::from(name.clone());
                let config = self.zones.zone_config(&fqdn);

                for rdata in alias_query_state.resolve_name(alias) {
                    if rdata.record_type() == query_state.query_type() {
                        records.push(rr::Record::from_rdata(name.clone(), config.ttl, rdata));
                    }
                }
            }

            query_state.add_answers(records);
        }

        let span = Span::current();
        span.record("response_code", query_state.response_code.to_str());
    }
}

pub(crate) struct DnsServer {
    server_state: ServerState<Zones>,
    server: ServerFuture<Handler>,
}

impl DnsServer {
    pub(crate) async fn new(
        server_config: &ServerConfig,
        server_state: ServerState<Zones>,
    ) -> Self {
        Self {
            server_state: server_state.clone(),
            server: Self::build_server(server_config, server_state).await,
        }
    }

    pub(crate) async fn shutdown(&mut self) {
        tracing::debug!("Shutting down DNS service");

        if let Err(e) = self.server.shutdown_gracefully().await {
            tracing::error!(error = %e, "Failure while shutting down DNS server.");
        }
    }

    pub(crate) async fn restart(&mut self, server_config: &ServerConfig) {
        tracing::debug!("Restarting DNS service");

        if let Err(e) = self.server.block_until_done().await {
            tracing::error!(error = %e, "Failure while shutting down DNS server.");
        }

        self.server = Self::build_server(server_config, self.server_state.clone()).await;
    }

    async fn build_server(
        server_config: &ServerConfig,
        server_state: ServerState<Zones>,
    ) -> ServerFuture<Handler> {
        let handler = Handler { server_state };

        let port = server_config.port.unwrap_or(53);

        let mut server = ServerFuture::new(handler);

        match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                tracing::info!("Server listening on udp://0.0.0.0:{}", port);
                server.register_socket(socket);
            }
            Err(e) => tracing::error!(error = %e, "Unable to open UDP socket"),
        }

        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(socket) => {
                tracing::info!("Server listening on tcp://0.0.0.0:{}", port);
                server.register_listener(socket, Duration::from_millis(500));
            }
            Err(e) => tracing::error!(error = %e, "Unable to open TCP socket"),
        }

        server
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_server::proto::{
        op::{Query, ResponseCode},
        rr::{DNSClass, RecordType},
    };
    use tokio::sync::watch::channel;

    use crate::{
        config::{ZoneConfig, ZoneConfigProvider},
        dns::{query::QueryState, Fqdn, RData, Record, RecordSet, ServerState, Upstream},
        test::{coredns_container, fqdn, name, rdata_a, rdata_aaaa, rdata_aname, rdata_cname},
        util::{Address, Host},
    };

    #[derive(Clone)]
    struct EmptyZones {}

    impl ZoneConfigProvider for EmptyZones {
        fn zone_config(&self, _: &Fqdn) -> ZoneConfig {
            Default::default()
        }
    }

    #[derive(Clone)]
    struct ZoneWithUpstream {
        upstream: Upstream,
    }

    impl ZoneConfigProvider for ZoneWithUpstream {
        fn zone_config(&self, _: &Fqdn) -> ZoneConfig {
            ZoneConfig {
                origin: None,
                upstreams: [self.upstream.clone()].into(),
                ttl: 300,
                authoritative: true,
            }
        }
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn query() {
        let mut records = RecordSet::new();
        records.insert(Record::new(
            fqdn("test.home.local."),
            RData::Cname(fqdn("other.home.local.")),
        ));

        let (_, receiver) = channel(records.clone());

        let query = Query::query(name("test.home.local."), RecordType::A);

        let mut query_state = QueryState::new(query.clone(), false);
        let mut server_state = ServerState::new(receiver, EmptyZones {}).locked().await;
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NXDomain);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);
        assert!(query_state.additionals().is_empty());

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("test.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("other.home.local."));

        records.insert(Record::new(
            fqdn("other.home.local."),
            RData::A("10.10.45.23".parse().unwrap()),
        ));

        let mut query_state = QueryState::new(query.clone(), true);
        server_state.records = records.clone();
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 2);
        assert!(query_state.additionals().is_empty());

        let record = answers.get(1).unwrap();
        assert_eq!(*record.name(), name("test.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("other.home.local."));

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("other.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("10.10.45.23"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn aliases() {
        let mut records = RecordSet::new();
        records.insert(Record::new(
            fqdn("alias.home.local."),
            RData::Aname(fqdn("target.home.local.")),
        ));
        records.insert(Record::new(
            fqdn("cname.home.local."),
            RData::Cname(fqdn("alias.home.local.")),
        ));
        records.insert(Record::new(
            fqdn("target.home.local."),
            RData::A("10.4.2.5".parse().unwrap()),
        ));
        records.insert(Record::new(
            fqdn("target.home.local."),
            RData::Aaaa("2a00:1450:4009:81e::200e".parse().unwrap()),
        ));
        records.insert(Record::new(
            fqdn("alias2.home.local."),
            RData::Aname(fqdn("cname.home.local.")),
        ));
        records.insert(Record::new(
            fqdn("badalias.home.local."),
            RData::Aname(fqdn("unknown.home.local.")),
        ));

        let (_, receiver) = channel(records.clone());

        let server_state = ServerState::new(receiver, EmptyZones {}).locked().await;

        let mut query_state = QueryState::new(
            Query::query(name("alias.home.local."), RecordType::A),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("10.4.2.5"));

        let mut query_state = QueryState::new(
            Query::query(name("alias.home.local."), RecordType::AAAA),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let answers = query_state.answers();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::AAAA);
        assert_eq!(
            *record.data().unwrap(),
            rdata_aaaa("2a00:1450:4009:81e::200e")
        );

        let mut query_state = QueryState::new(
            Query::query(name("cname.home.local."), RecordType::A),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 2);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("10.4.2.5"));

        let record = answers.get(1).unwrap();
        assert_eq!(*record.name(), name("cname.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("alias.home.local."));

        let mut query_state = QueryState::new(
            Query::query(name("alias2.home.local."), RecordType::AAAA),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias2.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::AAAA);
        assert_eq!(
            *record.data().unwrap(),
            rdata_aaaa("2a00:1450:4009:81e::200e")
        );

        let mut query_state = QueryState::new(
            Query::query(name("cname.home.local."), RecordType::CNAME),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("cname.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("alias.home.local."));

        let mut query_state = QueryState::new(
            Query::query(name("alias.home.local."), RecordType::ANAME),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::ANAME);
        assert_eq!(*record.data().unwrap(), rdata_aname("target.home.local."));

        let mut query_state = QueryState::new(
            Query::query(name("cname.home.local."), RecordType::ANAME),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 2);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::ANAME);
        assert_eq!(*record.data().unwrap(), rdata_aname("target.home.local."));

        let record = answers.get(1).unwrap();
        assert_eq!(*record.name(), name("cname.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("alias.home.local."));

        let mut query_state = QueryState::new(
            Query::query(name("badalias.home.local."), RecordType::AAAA),
            false,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NXDomain);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 0);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn upstream_overrides() {
        let coredns = coredns_container(
            "example.org",
            r#"
$ORIGIN example.org.
@   3600 IN	SOA sns.dns.icann.org. noc.dns.icann.org. 2024102601 7200 3600 1209600 3600
    3600 IN NS a.iana-servers.net.
    3600 IN NS b.iana-servers.net.

www     IN A     10.10.10.5
        IN AAAA  2001::1
alias   IN A     100.100.23.12
data    IN CNAME www
other   IN A     10.5.3.2
"#,
        )
        .await;

        let upstream = Upstream::from(Address {
            host: Host::from_str("127.0.0.1").unwrap(),
            port: Some(coredns.get_udp_port(53).await),
        });

        let mut records = RecordSet::new();
        records.insert(Record::new(
            fqdn("alias.example.org."),
            RData::Aname(fqdn("other.example.org.")),
        ));
        records.insert(Record::new(
            fqdn("alias2.example.org."),
            RData::Aname(fqdn("www.example.org.")),
        ));
        records.insert(Record::new(
            fqdn("other.example.org."),
            RData::A("100.230.45.23".parse().unwrap()),
        ));

        let (_, receiver) = channel(records.clone());

        let server_state = ServerState::new(receiver, ZoneWithUpstream { upstream })
            .locked()
            .await;

        let mut query_state = QueryState::new(
            Query::query(name("alias.example.org."), RecordType::A),
            true,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias.example.org."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("100.230.45.23"));

        let mut query_state = QueryState::new(
            Query::query(name("alias2.example.org."), RecordType::A),
            true,
        );
        server_state.perform_query(&mut query_state).await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("alias2.example.org."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("10.10.10.5"));
    }
}
