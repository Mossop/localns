use std::{collections::HashSet, time::Instant};

use hickory_server::{
    proto::{
        op::{Header, Query, ResponseCode},
        rr::{self, Name},
    },
    server::Request,
};
use log::Level;

use crate::config::Config;

use super::{Fqdn, RecordSet};

pub(super) struct QueryContext<'a> {
    request: &'a Request,

    seen: HashSet<Name>,
    unknowns: HashSet<Name>,

    recursion_available: bool,
    response_code: ResponseCode,

    answers: Vec<rr::Record>,
    additionals: Vec<rr::Record>,
    name_servers: Vec<rr::Record>,
    soa: Option<rr::Record>,
}

impl<'a> QueryContext<'a> {
    pub fn new(request: &'a Request) -> Self {
        let mut context = Self {
            request,

            seen: HashSet::new(),
            unknowns: HashSet::new(),

            recursion_available: true,
            response_code: ResponseCode::NoError,

            answers: Vec::new(),
            additionals: Vec::new(),
            name_servers: Vec::new(),
            soa: None,
        };

        let name = context.query().name().clone();
        context.seen.insert(name.clone());
        context.unknowns.insert(name);
        context
    }

    fn query(&self) -> &Query {
        self.request.request_info().query.original()
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
        if self.request.recursion_desired() {
            if let Some(rr::RData::CNAME(ref name)) = record.data() {
                if !self.seen.contains(name) {
                    self.seen.insert(name.0.clone());
                    self.unknowns.insert(name.0.clone());
                }
            }
        }
    }

    pub fn add_answers(&mut self, records: Vec<rr::Record>) {
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

    pub fn add_additionals(&mut self, records: Vec<rr::Record>) {
        self.additionals.extend(records);
    }

    pub fn add_name_servers(&mut self, records: Vec<rr::Record>) {
        self.name_servers.extend(records);
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

    pub async fn perform_query(&mut self, server: &Server) {
        let start = Instant::now();

        let mut is_first = true;
        let query_class = self.query().query_class();
        let query_type = self.query().query_type();
        let recurse = self.request.recursion_desired();

        while let Some(name) = self.next_unknown() {
            let fqdn = Fqdn::from(name.clone());
            let config = server.config.zone_config(&fqdn);
            log::trace!("Searching for {} with config {:?}", name, config);

            let records: Vec<rr::Record> = server
                .records
                .lookup(&name, query_class, query_type, recurse)
                .filter_map(|r| r.raw(&config))
                .collect();

            if !records.is_empty() {
                if is_first {
                    self.response_code = ResponseCode::NoError;
                    self.soa = config.soa();
                }

                self.add_answers(records);
            } else if let Some(upstream) = &config.upstream {
                if let Some(response) = upstream
                    .lookup(self.request.id(), &name, query_class, query_type)
                    .await
                {
                    let mut message = response.into_message();

                    if is_first {
                        match message.response_code() {
                            ResponseCode::NXDomain => {
                                if server.records.has_name(&name) {
                                    self.response_code = ResponseCode::NoError;
                                } else {
                                    self.response_code = ResponseCode::NXDomain;
                                }
                            }
                            code => self.response_code = code,
                        }

                        self.recursion_available = message.recursion_available();

                        self.add_answers(message.take_answers());
                        self.add_additionals(message.take_additionals());

                        let mut name_servers: Vec<rr::Record> = Vec::new();
                        let mut soa: Option<rr::Record> = None;

                        for record in message.take_name_servers() {
                            if record.record_type() == rr::RecordType::SOA {
                                soa.replace(record);
                            } else {
                                name_servers.push(record);
                            }
                        }

                        self.add_name_servers(name_servers);
                        self.soa = config.soa().or(soa);
                    } else {
                        self.add_additionals(message.take_answers());
                        self.add_additionals(message.take_additionals());
                    }
                }
            }

            is_first = false;
        }

        let request_info = self.request.request_info();
        let mut rflags = Vec::new();
        if self.soa.is_some() {
            rflags.push("AA");
        }
        if self.recursion_available {
            rflags.push("RA");
        }

        let duration = Instant::now() - start;

        let level = match self.response_code {
            ResponseCode::NoError => Level::Trace,
            ResponseCode::NXDomain => Level::Debug,
            _ => Level::Warn,
        };

        log::log!(level, "({id}) Query src:{proto}://{addr}#{port} {query}:{qtype}:{class} qflags:{qflags} response:{code:?} rr:{answers}/{authorities}/{additionals} rflags:{rflags} ms:{duration}",
            id = self.request.id(),
            proto = request_info.protocol,
            addr = request_info.src.ip(),
            port = request_info.src.port(),
            query = self.query().name(),
            qtype = self.query().query_type(),
            class = self.query().query_class(),
            qflags = request_info.header.flags(),
            code = self.response_code,
            answers = self.answers.len(),
            authorities = self.name_servers.len(),
            additionals = self.additionals.len(),
            rflags = rflags.join(","),
            duration = duration.as_millis(),
        );
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
}
