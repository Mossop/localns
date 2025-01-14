use std::{
    collections::{HashMap, HashSet},
    iter::once,
};

use hickory_server::proto::{
    op::{Header, Query, ResponseCode},
    rr::{self, DNSClass, Name, RData, RecordType},
};

pub(super) struct QueryState {
    /// The original query.
    pub(super) query: Query,
    /// Whether recursion was requested.
    pub(super) recursion_desired: bool,
    /// Whether to directly resolve aliases.
    pub(super) resolve_aliases: bool,

    /// A list of names that we have already seen
    seen: HashSet<Name>,
    /// A list of names that remain to be looked up
    unknowns: HashSet<Name>,

    pub(super) recursion_available: bool,
    pub(super) response_code: ResponseCode,

    /// A list of answers to respond with
    answers: Vec<rr::Record>,
    /// A list of additional records.
    additionals: Vec<rr::Record>,
    /// A list of found alias records that must be resolved.
    pub(super) aliases: HashMap<Name, Name>,
    pub(super) name_servers: Vec<rr::Record>,
    pub(super) soa: Option<rr::Record>,
}

impl QueryState {
    pub(super) fn new(query: Query, recursion_desired: bool) -> Self {
        QueryState {
            seen: HashSet::from_iter(once(query.name().clone())),
            unknowns: HashSet::from_iter(once(query.name().clone())),

            query,
            recursion_desired,
            resolve_aliases: false,

            recursion_available: true,
            response_code: ResponseCode::NXDomain,

            answers: Vec::new(),
            additionals: Vec::new(),
            aliases: HashMap::new(),
            name_servers: Vec::new(),
            soa: None,
        }
    }

    pub(super) fn for_aliases(&mut self) -> Self {
        QueryState {
            seen: self.seen.clone(),
            unknowns: self
                .aliases
                .values()
                .filter(|alias| !self.seen.contains(alias))
                .cloned()
                .collect(),

            query: self.query.clone(),
            recursion_desired: self.recursion_desired,
            resolve_aliases: true,

            recursion_available: true,
            response_code: ResponseCode::NXDomain,

            answers: self.answers.clone(),
            additionals: self.additionals.clone(),
            aliases: HashMap::new(),
            name_servers: Vec::new(),
            soa: None,
        }
    }

    pub(super) fn resolve_name(&self, name: &Name) -> impl Iterator<Item = RData> {
        let mut rdata_results = Vec::new();

        for record in self.answers.iter().chain(self.additionals.iter()) {
            if record.name() == name {
                if let Some(rdata) = record.data() {
                    match rdata {
                        RData::CNAME(cname) => rdata_results.extend(self.resolve_name(&cname.0)),
                        rdata => {
                            rdata_results.push(rdata.clone());
                        }
                    }
                }
            }
        }

        rdata_results.into_iter()
    }

    pub(super) fn query_type(&self) -> RecordType {
        self.query.query_type()
    }

    pub(super) fn query_class(&self) -> DNSClass {
        self.query.query_class()
    }

    pub(super) fn answers(&self) -> &Vec<rr::Record> {
        &self.answers
    }

    pub(super) fn additionals(&self) -> &Vec<rr::Record> {
        &self.additionals
    }

    pub(super) fn name_servers(&self) -> &Vec<rr::Record> {
        &self.name_servers
    }

    pub(super) fn soa(&self) -> &Option<rr::Record> {
        &self.soa
    }

    fn add_unknowns(&mut self, record: &rr::Record) {
        if let Some(rr::RData::CNAME(ref name)) = record.data() {
            if !self.seen.contains(name) {
                self.seen.insert(name.0.clone());
                self.unknowns.insert(name.0.clone());
            }
        }
    }

    pub(super) fn add_answers(&mut self, records: Vec<rr::Record>) {
        for record in &records {
            self.seen.insert(record.name().clone());
            self.unknowns.remove(record.name());
            self.add_unknowns(record);

            if record.record_type() == self.query.query_type() {
                self.response_code = ResponseCode::NoError;
            }
        }

        self.answers.extend(records);
    }

    pub(super) fn add_additionals(&mut self, records: Vec<rr::Record>) {
        self.additionals.extend(records);
    }

    pub(super) fn next_unknown(&mut self) -> Option<Name> {
        let next = self.unknowns.iter().next()?.clone();
        self.unknowns.remove(&next);
        Some(next)
    }

    pub(super) fn header(&self, request_header: &Header) -> Header {
        let mut response_header = Header::response_from_request(request_header);
        response_header.set_authoritative(self.soa.is_some());
        response_header.set_recursion_available(self.recursion_available);
        response_header.set_response_code(self.response_code);
        response_header
    }
}
