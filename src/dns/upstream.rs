use std::{fmt, net::SocketAddr};

use hickory_client::{
    client::{AsyncClient, ClientHandle},
    op::DnsResponse,
    rr::{self, DNSClass, Name, RecordType},
    udp::UdpClientStream,
};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tracing::{instrument, Span};

use crate::{dns::query::QueryState, util::Address, Error};

type UpstreamConfig = Address;

async fn connect_client(address: SocketAddr) -> Result<AsyncClient, Error> {
    let stream = UdpClientStream::<UdpSocket>::new(address);

    let client = AsyncClient::connect(stream);
    let (client, bg) = client.await?;
    tokio::spawn(bg);

    Ok(client)
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "UpstreamConfig")]
pub(crate) struct Upstream {
    pub(crate) config: UpstreamConfig,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&format!("{}", self.config))
    }
}

impl From<UpstreamConfig> for Upstream {
    fn from(config: UpstreamConfig) -> Upstream {
        Upstream { config }
    }
}

impl Upstream {
    #[instrument(fields(
        lookup.upstream = %self.config,
        lookup.name = %name,
        lookup.query_class = %query_class,
        lookup.query_type = %query_type,
        lookup.response_code,
    ), skip_all)]
    async fn lookup(
        &self,
        name: &Name,
        query_class: DNSClass,
        query_type: RecordType,
    ) -> Option<DnsResponse> {
        let address = match self.config.to_socket_address(53) {
            Ok(addr) => addr,
            Err(e) => {
                tracing::error!(error = %e, "Unable to lookup nameserver");
                return None;
            }
        };

        let mut client = match connect_client(address).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e);
                return None;
            }
        };

        let result = client.query(name.clone(), query_class, query_type).await;

        match result {
            Ok(response) => {
                let span = Span::current();
                span.record("lookup.response_code", response.response_code().to_string());
                Some(response)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Upstream DNS server returned error");
                None
            }
        }
    }

    pub(super) async fn resolve(&self, name: &Name, query_state: &mut QueryState) {
        if let Some(response) = self
            .lookup(name, query_state.query_class(), query_state.query_type())
            .await
        {
            let mut message = response.into_message();

            query_state.add_answers(message.take_answers());
            query_state.add_additionals(message.take_additionals());

            if name == query_state.query.name() {
                query_state.response_code = message.response_code();
                query_state.recursion_available = message.recursion_available();

                let mut name_servers: Vec<rr::Record> = Vec::new();
                let mut soa: Option<rr::Record> = None;

                for record in message.take_name_servers() {
                    if record.record_type() == rr::RecordType::SOA {
                        soa.replace(record);
                    } else {
                        name_servers.push(record);
                    }
                }

                query_state.name_servers.extend(name_servers);
                query_state.soa = soa;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hickory_client::{
        op::{Query, ResponseCode},
        rr::{DNSClass, RecordType},
    };

    use crate::{
        dns::{query::QueryState, Upstream},
        test::{coredns_container, name, rdata_a, rdata_cname},
        util::{Address, Host},
    };

    #[tokio::test]
    async fn test_upstream() {
        let coredns = coredns_container(
            "example.org",
            r#"
$ORIGIN example.org.
@   3600 IN	SOA sns.dns.icann.org. noc.dns.icann.org. 2024102601 7200 3600 1209600 3600
    3600 IN NS a.iana-servers.net.
    3600 IN NS b.iana-servers.net.

www     IN A     10.10.10.5
        IN AAAA  2001::1
data    IN CNAME www
"#,
        )
        .await;

        let upstream = Upstream::from(Address {
            host: Host::from("127.0.0.1"),
            port: Some(coredns.get_udp_port(53).await),
        });

        let mut query_state = QueryState::new(
            Query::query(name("unknown.example.org."), RecordType::A),
            false,
        );
        upstream
            .resolve(&name("unknown.example.org."), &mut query_state)
            .await;

        assert_eq!(query_state.response_code, ResponseCode::NXDomain);
        assert!(query_state.answers().is_empty());
        assert!(query_state.additionals().is_empty());

        let mut query_state =
            QueryState::new(Query::query(name("www.example.org."), RecordType::A), false);
        upstream
            .resolve(&name("www.example.org."), &mut query_state)
            .await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        assert!(query_state.additionals().is_empty());
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 1);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("www.example.org."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(*record.data().unwrap(), rdata_a("10.10.10.5"));

        let mut query_state = QueryState::new(
            Query::query(name("data.example.org."), RecordType::A),
            false,
        );
        upstream
            .resolve(&name("data.example.org."), &mut query_state)
            .await;

        assert_eq!(query_state.response_code, ResponseCode::NoError);
        assert!(query_state.additionals().is_empty());
        let mut answers = query_state.answers().clone();
        answers.sort();
        assert_eq!(answers.len(), 2);

        let mut answers = answers.into_iter();
        let record = answers.next().unwrap();
        assert_eq!(*record.name(), name("data.example.org."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(*record.data().unwrap(), rdata_cname("www.example.org."));

        let record = answers.next().unwrap();
        assert_eq!(*record.name(), name("www.example.org."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(*record.data().unwrap(), rdata_a("10.10.10.5"));
    }
}
