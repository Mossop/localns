use std::{collections::HashSet, iter::once};

use hickory_server::proto::{
    op::{Header, Query, ResponseCode},
    rr::{self, DNSClass, Name, RecordType},
};

pub(super) struct QueryState {
    /// The original query.
    pub(super) query: Query,
    /// Whether recursion was requested.
    pub(super) recursion_desired: bool,

    /// A list of names that we have already seen
    seen: HashSet<Name>,
    /// A list of names that remain to be looked up
    unknowns: HashSet<Name>,

    pub(super) recursion_available: bool,
    pub(super) response_code: ResponseCode,

    /// A list of answers to respond with
    answers: Vec<rr::Record>,
    /// A list of additional records such as resolved CNAMEs.
    additionals: Vec<rr::Record>,
    pub(super) name_servers: Vec<rr::Record>,
    pub(super) soa: Option<rr::Record>,
}

impl QueryState {
    pub(super) fn new(query: Query, recursion_desired: bool) -> Self {
        QueryState {
            seen: HashSet::from_iter(once(query.name().clone())),
            unknowns: HashSet::new(),

            query,
            recursion_desired,

            recursion_available: true,
            response_code: ResponseCode::NXDomain,

            answers: Vec::new(),
            additionals: Vec::new(),
            name_servers: Vec::new(),
            soa: None,
        }
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
        if self.recursion_desired {
            if let Some(rr::RData::CNAME(ref name)) = record.data() {
                if !self.seen.contains(name) {
                    self.seen.insert(name.0.clone());
                    self.unknowns.insert(name.0.clone());
                }
            }
        }
    }

    pub(super) fn add_answers(&mut self, records: Vec<rr::Record>) {
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
