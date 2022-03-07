use std::{
    collections::HashSet,
    hash::Hash,
    hash::Hasher,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use trust_dns_server::{client::rr::RData, resolver::Name};

#[derive(Debug, Eq, Clone)]
pub struct Record {
    pub name: Name,
    pub ttl: Option<u32>,
    pub data: RData,
}

impl Record {
    pub fn new(name: Name, data: RData) -> Self {
        Self {
            name,
            data,
            ttl: None,
        }
    }
}

impl Hash for Record {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.ttl.hash(state);
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.ttl == other.ttl // && self.data == other.data
    }
}

pub fn fqdn(name: &str) -> Name {
    let mut name = Name::parse(name, None).unwrap();
    name.set_fqdn(true);
    name
}

pub fn rdata(name: &str) -> RData {
    if let Ok(ip) = Ipv4Addr::from_str(name) {
        RData::A(ip)
    } else if let Ok(ip) = Ipv6Addr::from_str(name) {
        RData::AAAA(ip)
    } else {
        RData::CNAME(Name::parse(name, None).unwrap())
    }
}

pub type RecordSet = HashSet<Record>;
