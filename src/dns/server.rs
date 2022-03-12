use std::collections::HashSet;

use trust_dns_server::client::{
    op::{Header, Query, ResponseCode},
    rr::{self, Name},
};

use crate::config::Config;

use super::{Fqdn, RecordSet};

#[derive(Default)]
pub(super) struct QueryState {
    seen: HashSet<Name>,
    unknowns: HashSet<Name>,

    recursion_available: bool,
    response_code: ResponseCode,

    answers: Vec<rr::Record>,
    additionals: Vec<rr::Record>,
    name_servers: Vec<rr::Record>,
    soa: Option<rr::Record>,
}

impl QueryState {
    pub fn new(name: Name) -> Self {
        let mut state = Self {
            recursion_available: true,
            response_code: ResponseCode::NXDomain,
            ..Default::default()
        };

        state.seen.insert(name.clone());
        state.unknowns.insert(name);
        state
    }

    pub fn answers(&self) -> impl Iterator<Item = &rr::Record> {
        self.answers.iter()
    }

    pub fn additionals(&self) -> impl Iterator<Item = &rr::Record> {
        self.additionals.iter()
    }

    pub fn name_servers(&self) -> impl Iterator<Item = &rr::Record> {
        self.name_servers.iter()
    }

    pub fn soa(&self) -> impl Iterator<Item = &rr::Record> {
        self.soa.iter()
    }

    fn add_unknowns(&mut self, record: &rr::Record) {
        if let Some(rr::RData::CNAME(ref name)) = record.data() {
            if !self.seen.contains(name) {
                self.seen.insert(name.clone());
                self.unknowns.insert(name.clone());
            }
        }
    }

    pub fn set_recursion_available(&mut self, recursion_available: bool) {
        self.recursion_available = recursion_available;
    }

    pub fn set_response_code(&mut self, response_code: ResponseCode) {
        self.response_code = response_code;
    }

    pub fn add_answers(&mut self, records: Vec<rr::Record>) {
        if !records.is_empty() && self.response_code == ResponseCode::NXDomain {
            self.response_code = ResponseCode::NoError;
        }

        for record in records {
            self.seen.insert(record.name().clone());
            self.unknowns.remove(record.name());
            self.add_unknowns(&record);

            self.answers.push(record);
        }
    }

    pub fn add_additionals(&mut self, records: Vec<rr::Record>) {
        for record in records {
            self.seen.insert(record.name().clone());
            self.unknowns.remove(record.name());
            self.add_unknowns(&record);

            self.additionals.push(record);
        }
    }

    pub fn add_name_servers(&mut self, records: Vec<rr::Record>) {
        self.name_servers.extend(records);
    }

    pub fn set_soa(&mut self, soa: Option<rr::Record>) {
        self.soa = soa;
    }

    pub fn next_unknown(&mut self) -> Option<Name> {
        let next = self.unknowns.iter().next()?.clone();
        self.unknowns.remove(&next);
        Some(next)
    }

    pub fn header(&self, request_header: &Header) -> Header {
        let mut response_header = Header::response_from_request(request_header);
        response_header.set_authoritative(self.soa.is_some());
        response_header.set_recursion_available(self.recursion_available);
        response_header.set_response_code(self.response_code);
        response_header
    }
}

#[derive(Debug, Clone)]
pub(super) struct Server {
    records: RecordSet,
    config: Config,
}

impl Server {
    pub fn new(config: Config, records: RecordSet) -> Self {
        Self { config, records }
    }

    pub fn update_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn update_records(&mut self, records: RecordSet) {
        self.records = records;
    }

    pub async fn query(&self, query: &Query, _recurse: bool) -> QueryState {
        log::debug!(
            "Starting new query for {} class {} type {}",
            query.name(),
            query.query_class(),
            query.query_type()
        );

        let mut state = QueryState::new(query.name().clone());
        state.set_recursion_available(true);

        let mut is_first = true;
        while let Some(name) = state.next_unknown() {
            let fqdn = Fqdn::from(name.clone());
            let config = self.config.zone_config(&fqdn);
            log::trace!("Searching for {} with config {:?}", name, config);

            let records: Vec<rr::Record> = self
                .records
                .lookup(&name, query.query_class(), query.query_type())
                .filter_map(|r| r.raw(&config))
                .collect();

            if !records.is_empty() {
                if is_first {
                    state.add_answers(records);
                    state.set_soa(config.soa())
                } else {
                    state.add_additionals(records);
                }
            } else if let Some(upstream) = &config.upstream {
                if let Some(mut response) = upstream
                    .lookup(&name, query.query_class(), query.query_type())
                    .await
                {
                    if is_first {
                        state.set_response_code(response.response_code());
                        state.set_recursion_available(response.recursion_available());

                        state.add_answers(response.take_answers());
                        state.add_additionals(response.take_additionals());

                        let mut name_servers: Vec<rr::Record> = Vec::new();
                        let mut soa: Option<rr::Record> = None;

                        for record in response.take_name_servers() {
                            if record.record_type() == rr::RecordType::SOA {
                                soa.replace(record);
                            } else {
                                name_servers.push(record);
                            }
                        }

                        state.add_name_servers(name_servers);
                        state.set_soa(config.soa().or(soa));
                    } else {
                        state.add_additionals(response.take_answers());
                        state.add_additionals(response.take_additionals());
                    }
                }
            }

            is_first = false;
        }

        state
    }
}
