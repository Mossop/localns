use std::{
    collections::HashSet,
    hash::Hash,
    hash::Hasher,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::{Arc, Mutex},
};

pub use trust_dns_server::client::rr::{Name, RData};

use crate::config::Host;

#[derive(Debug, Eq, Clone)]
pub struct Record {
    pub name: Name,
    pub ttl: Option<u32>,
    pub data: RData,
}

impl Record {
    pub fn new(name: Name, data: RData) -> Self {
        if let RData::CNAME(ref alias) = data {
            if &name == alias {
                panic!("Attempted to create a CNAME cycle with {}", name);
            }
        }

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

fn resolve(records: &RecordSet, name: Name) -> Option<Host> {
    for record in records.iter() {
        if record.name == name {
            match &record.data {
                RData::A(ip) => return Some(Host::Ipv4(*ip)),
                RData::AAAA(ip) => return Some(Host::Ipv6(*ip)),
                RData::CNAME(name) => return Some(name.into()),
                _ => return None,
            }
        }
    }

    None
}

#[derive(Clone)]
pub struct SharedRecordSet {
    records: Arc<Mutex<RecordSet>>,
}

impl SharedRecordSet {
    pub fn new(records: RecordSet) -> Self {
        Self {
            records: Arc::new(Mutex::new(records)),
        }
    }

    pub fn add_records(&self, new_records: RecordSet) {
        let mut records = self.records.lock().unwrap();

        for record in new_records {
            records.insert(record);
        }
    }

    pub fn replace_records(&self, new_records: RecordSet) {
        let mut records = self.records.lock().unwrap();

        records.clear();
        for record in new_records {
            records.insert(record);
        }
    }

    pub fn resolve(&self, address: &Host) -> Host {
        let resolved = if let Host::Name(hostname) = address {
            let name = fqdn(hostname);
            let records = self.records.lock().unwrap();

            resolve(&records, name)
        } else {
            return address.clone();
        };

        if let Some(host) = resolved {
            self.resolve(&host)
        } else {
            address.clone()
        }
    }
}
