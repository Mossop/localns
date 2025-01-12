use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::Error;
use futures::FutureExt;
use hickory_server::{
    proto::{
        op::Query,
        rr::{self, Name, RecordType},
    },
    ServerFuture,
};
use serde::Deserialize;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::RwLock,
};
use tracing::{instrument, Span};

mod handler;
mod query;
mod record;
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
    pub(crate) records: Arc<RwLock<RecordSet>>,
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
    pub(crate) fn new(records: RecordSet, zones: Z) -> Self {
        Self {
            records: Arc::new(RwLock::new(records)),
            zones: Arc::new(RwLock::new(zones)),
        }
    }

    pub(crate) async fn replace_records(&self, records: RecordSet) {
        let mut locked = self.records.write().await;
        *locked = records;
    }

    pub(crate) async fn replace_zones(&self, zones: Z) {
        let mut locked = self.zones.write().await;
        *locked = zones;
    }

    pub(crate) async fn locked(&self) -> LockedServerState<Z> {
        let zones = self.zones.read().await.clone();
        let records = self.records.read().await.clone();

        LockedServerState { zones, records }
    }
}

impl<Z: ZoneConfigProvider> LockedServerState<Z> {
    #[instrument(skip(self))]
    async fn resolve_http_address(&self, name: String) -> Result<Vec<SocketAddr>, Error> {
        let mut name = Name::from_str(&name)?;
        name.set_fqdn(true);

        let mut results = Vec::<SocketAddr>::new();

        let query = Query::query(name.clone(), RecordType::A);
        let mut query_state = QueryState::new(query, true);
        self.perform_query(&mut query_state).await;
        results.extend(query_state.resolve_name(&name));

        let query = Query::query(name.clone(), RecordType::AAAA);
        let mut query_state = QueryState::new(query, true);
        self.perform_query(&mut query_state).await;
        results.extend(query_state.resolve_name(&name));

        Ok(results)
    }

    async fn lookup_name(&self, name: &Name, query_state: &mut QueryState) {
        let fqdn = Fqdn::from(name.clone());
        let config = self.zones.zone_config(&fqdn);
        tracing::trace!(name = %name, config = ?config, "Looking up name");

        let records: Vec<rr::Record> = self
            .records
            .lookup(name, query_state.query_class(), query_state.query_type())
            .filter_map(|r| r.raw(&config))
            .collect();

        if !config.upstreams.is_empty() && name == query_state.query.name() {
            query_state.recursion_available = true;
        }

        if !records.is_empty() {
            query_state.add_answers(records);

            if name == query_state.query.name() {
                query_state.soa = config.soa();
            }

            return;
        };

        if query_state.recursion_desired {
            for upstream in &config.upstreams {
                upstream.resolve(name, query_state).await;
            }
        }
    }

    #[instrument(fields(
        query = %query_state.query.name(),
        qtype = query_state.query.query_type().to_string(),
        class = query_state.query.query_class().to_string(),
        request.response_code,
    ), skip(self, query_state))]
    pub(crate) async fn perform_query(&self, query_state: &mut QueryState) {
        // Lookup the original name.
        self.lookup_name(&query_state.query.name().clone(), query_state)
            .await;

        // Now lookup any new names that were discovered.
        while let Some(name) = query_state.next_unknown() {
            self.lookup_name(&name, query_state).await;
        }

        let span = Span::current();
        span.record("request.response_code", query_state.response_code.to_str());
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
    use hickory_server::proto::{
        op::{Query, ResponseCode},
        rr::{DNSClass, RecordType},
    };

    use crate::{
        config::{ZoneConfig, ZoneConfigProvider},
        dns::{query::QueryState, Fqdn, RData, Record, RecordSet, ServerState},
        test::{fqdn, name, rdata_a, rdata_cname},
    };

    #[derive(Clone)]
    struct EmptyZones {}

    impl ZoneConfigProvider for EmptyZones {
        fn zone_config(&self, _: &Fqdn) -> ZoneConfig {
            Default::default()
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

        let query = Query::query(name("test.home.local."), RecordType::A);

        let mut query_state = QueryState::new(query.clone(), false);
        let mut server_state = ServerState::new(records.clone(), EmptyZones {})
            .locked()
            .await;
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
}
