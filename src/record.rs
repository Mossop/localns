use std::{
    collections::HashSet,
    hash::Hash,
    hash::Hasher,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

pub use trust_dns_server::client::rr::{Name, RData};

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

        match self.data {
            RData::A(ref ip) => {
                "a".hash(state);
                ip.hash(state);
            }
            RData::AAAA(ref ip) => {
                "aaaa".hash(state);
                ip.hash(state);
            }
            RData::ANAME(_) => todo!(),
            RData::CAA(_) => todo!(),
            RData::CNAME(ref name) => {
                "cname".hash(state);
                name.hash(state);
            }
            RData::CSYNC(_) => todo!(),
            RData::HINFO(_) => todo!(),
            RData::HTTPS(_) => todo!(),
            RData::MX(_) => todo!(),
            RData::NAPTR(_) => todo!(),
            RData::NULL(_) => todo!(),
            RData::NS(_) => todo!(),
            RData::OPENPGPKEY(_) => todo!(),
            RData::OPT(_) => todo!(),
            RData::PTR(_) => todo!(),
            RData::SOA(_) => todo!(),
            RData::SRV(_) => todo!(),
            RData::SSHFP(_) => todo!(),
            RData::SVCB(_) => todo!(),
            RData::TLSA(_) => todo!(),
            RData::TXT(_) => todo!(),
            RData::Unknown { code: _, rdata: _ } => todo!(),
            // RData::ZERO => todo!(),
            _ => todo!(),
        }
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.ttl == other.ttl && self.data == other.data
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
        RData::CNAME(fqdn(name))
    }
}

pub type RecordSet = HashSet<Record>;
