use std::collections::HashSet;

use hickory_server::proto::{
    op::{Header, Query, ResponseCode},
    rr::{self, DNSClass, Name, RecordType},
};
use tracing::{instrument, Span};

use crate::{config::ZoneConfigProvider, dns::RecordSet};

use super::Fqdn;

pub(super) struct QueryContext<Z> {
    records: RecordSet,
    zones: Z,

    /// A list of names that we have already seen
    seen: HashSet<Name>,
    /// A list of names that remain to be looked up
    unknowns: HashSet<Name>,

    recursion_desired: bool,
    recursion_available: bool,
    response_code: ResponseCode,

    /// A list of answers to respond with
    answers: Vec<rr::Record>,
    /// A list of additional records such as resolved CNAMEs.
    additionals: Vec<rr::Record>,
    name_servers: Vec<rr::Record>,
    soa: Option<rr::Record>,
}

impl<Z: ZoneConfigProvider> QueryContext<Z> {
    pub(crate) fn new(records: RecordSet, zones: Z) -> Self {
        Self {
            records,
            zones,

            seen: HashSet::new(),
            unknowns: HashSet::new(),

            recursion_desired: false,
            recursion_available: true,
            response_code: ResponseCode::NoError,

            answers: Vec::new(),
            additionals: Vec::new(),
            name_servers: Vec::new(),
            soa: None,
        }
    }

    pub(crate) fn answers(&self) -> impl Iterator<Item = &rr::Record> {
        self.answers.iter()
    }

    pub(crate) fn additionals(&self) -> impl Iterator<Item = &rr::Record> {
        self.additionals.iter()
    }

    pub(crate) fn name_servers(&self) -> impl Iterator<Item = &rr::Record> {
        self.name_servers.iter()
    }

    pub(crate) fn soa(&self) -> impl Iterator<Item = &rr::Record> {
        self.soa.iter()
    }

    fn add_unknowns(&mut self, record: &rr::Record) {
        if self.recursion_desired {
            if let Some(rr::RData::CNAME(ref name)) = record.data() {
                if !self.seen.contains(name) {
                    self.seen.insert(name.0.clone());
                    self.unknowns.insert(name.0.clone());
                }
            }
        }
    }

    fn add_answers(&mut self, records: Vec<rr::Record>) {
        if !records.is_empty() && self.response_code == ResponseCode::NXDomain {
            self.response_code = ResponseCode::NoError;
        }

        for record in &records {
            self.seen.insert(record.name().clone());
            self.unknowns.remove(record.name());
            self.add_unknowns(record);
        }

        self.answers.extend(records);
    }

    fn add_additionals(&mut self, records: Vec<rr::Record>) {
        self.additionals.extend(records);
    }

    fn next_unknown(&mut self) -> Option<Name> {
        let next = self.unknowns.iter().next()?.clone();
        self.unknowns.remove(&next);
        Some(next)
    }

    pub(crate) fn header(&self, request_header: &Header) -> Header {
        let mut response_header = Header::response_from_request(request_header);
        response_header.set_authoritative(self.soa.is_some());
        response_header.set_recursion_available(self.recursion_available);
        response_header.set_response_code(self.response_code);
        response_header
    }

    async fn lookup_name(
        &mut self,
        original_name: &Name,
        name: &Name,
        query_class: DNSClass,
        query_type: RecordType,
    ) {
        let fqdn = Fqdn::from(name.clone());
        let config = self.zones.zone_config(&fqdn);
        tracing::trace!(name = %name, config = ?config, "Looking up name");

        let records: Vec<rr::Record> = self
            .records
            .lookup(name, query_class, query_type, self.recursion_desired)
            .filter_map(|r| r.raw(&config))
            .collect();

        if !records.is_empty() {
            self.add_answers(records);

            if name == original_name {
                self.soa = config.soa();
                self.response_code = ResponseCode::NoError;
            }
        } else if let Some(upstream) = &config.upstream {
            if let Some(response) = upstream.lookup(name, query_class, query_type).await {
                let mut message = response.into_message();

                self.add_answers(message.take_answers());
                self.add_additionals(message.take_additionals());

                if name == original_name {
                    match message.response_code() {
                        ResponseCode::NXDomain => {
                            if self.records.has_name(name) {
                                self.response_code = ResponseCode::NoError;
                            } else {
                                self.response_code = ResponseCode::NXDomain;
                            }
                        }
                        code => self.response_code = code,
                    }

                    self.recursion_available = message.recursion_available();

                    let mut name_servers: Vec<rr::Record> = Vec::new();
                    let mut soa: Option<rr::Record> = None;

                    for record in message.take_name_servers() {
                        if record.record_type() == rr::RecordType::SOA {
                            soa.replace(record);
                        } else {
                            name_servers.push(record);
                        }
                    }

                    self.name_servers.extend(name_servers);
                    self.soa = config.soa().or(soa);
                }
            }
        }
    }

    #[instrument(fields(
        query = %query.name(),
        qtype = query.query_type().to_string(),
        class = query.query_class().to_string(),
        request.response_code,
    ), skip(self))]
    pub(crate) async fn perform_query(&mut self, query: &Query, recursion_desired: bool) {
        self.seen.clear();
        self.unknowns.clear();
        self.recursion_desired = recursion_desired;
        self.recursion_available = true;
        self.response_code = ResponseCode::NXDomain;
        self.answers.clear();
        self.additionals.clear();
        self.name_servers.clear();
        self.soa = None;

        let original_name = query.name().clone();
        self.seen.insert(original_name.clone());

        let query_class = query.query_class();
        let query_type = query.query_type();

        // Lookup the original name.
        self.lookup_name(&original_name, &original_name, query_class, query_type)
            .await;

        // Now lookup any new names that were discovered.
        while let Some(name) = self.next_unknown() {
            self.lookup_name(&original_name, &name, query_class, query_type)
                .await;
        }

        let span = Span::current();
        span.record("request.response_code", self.response_code.to_str());
    }
}

#[cfg(test)]
mod tests {
    use hickory_server::proto::{
        op::{Query, ResponseCode},
        rr::{self, rdata, DNSClass, RecordType},
    };

    use crate::{
        config::{ZoneConfig, ZoneConfigProvider},
        dns::{server::QueryContext, Fqdn, RData, Record, RecordSet},
        test::name,
    };

    struct EmptyZones {}

    impl ZoneConfigProvider for EmptyZones {
        fn zone_config(&self, _: &Fqdn) -> ZoneConfig {
            Default::default()
        }
    }

    #[tokio::test]
    async fn query() {
        let mut records = RecordSet::new();
        records.insert(Record::new(
            Fqdn::from("test.home.local."),
            RData::Cname(Fqdn::from("other.home.local.")),
        ));

        let mut context = QueryContext::new(records.clone(), EmptyZones {});
        let query = Query::query(name("test.home.local."), RecordType::A);
        context.perform_query(&query, false).await;

        // No recursion will not return the CNAME record.
        assert_eq!(context.response_code, ResponseCode::NXDomain);
        assert_eq!(context.answers().next(), None);
        assert_eq!(context.additionals().next(), None);

        context.perform_query(&query, true).await;

        assert_eq!(context.response_code, ResponseCode::NoError);
        let mut answers = context.answers().collect::<Vec<&rr::Record>>();
        answers.sort();
        assert_eq!(answers.len(), 1);
        assert_eq!(context.additionals().next(), None);

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("test.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(
            *record.data().unwrap(),
            rr::RData::CNAME(rdata::CNAME(name("other.home.local.")))
        );

        records.insert(Record::new(
            Fqdn::from("other.home.local."),
            RData::A("10.10.45.23".parse().unwrap()),
        ));

        let mut context = QueryContext::new(records.clone(), EmptyZones {});
        context.perform_query(&query, true).await;

        assert_eq!(context.response_code, ResponseCode::NoError);
        let mut answers = context.answers().collect::<Vec<&rr::Record>>();
        answers.sort();
        assert_eq!(answers.len(), 2);
        assert_eq!(context.additionals().next(), None);

        let record = answers.get(1).unwrap();
        assert_eq!(*record.name(), name("test.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::CNAME);
        assert_eq!(
            *record.data().unwrap(),
            rr::RData::CNAME(rdata::CNAME(name("other.home.local.")))
        );

        let record = answers.first().unwrap();
        assert_eq!(*record.name(), name("other.home.local."));
        assert_eq!(record.dns_class(), DNSClass::IN);
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(
            *record.data().unwrap(),
            rr::RData::A(rdata::A("10.10.45.23".parse().unwrap()))
        );
    }
}
