use std::{
    collections::{
        hash_map::{IntoValues, Keys, Values},
        HashMap, HashSet,
    },
    fmt::{self, Display},
    hash::Hash,
    iter::Flatten,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use serde::Deserialize;
use trust_dns_server::{
    client::rr::{DNSClass, LowerName, Name, RecordType},
    proto::rr,
};

use crate::{config::ZoneConfig, util::upsert};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
#[serde(from = "String")]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Cname(Fqdn),
}

impl RData {
    pub fn data_type(&self) -> RecordType {
        match self {
            RData::A(_) => RecordType::A,
            RData::Aaaa(_) => RecordType::AAAA,
            RData::Cname(_) => RecordType::CNAME,
        }
    }

    pub fn is_valid(&self) -> bool {
        if let RData::Cname(name) = self {
            name.is_valid()
        } else {
            true
        }
    }
}

impl TryInto<rr::RData> for RData {
    type Error = String;

    fn try_into(self) -> Result<rr::RData, Self::Error> {
        match self {
            RData::A(ip) => Ok(rr::RData::A(ip)),
            RData::Aaaa(ip) => Ok(rr::RData::AAAA(ip)),
            RData::Cname(name) => match name {
                Fqdn::Valid(name) => Ok(rr::RData::CNAME(name)),
                Fqdn::Invalid(str) => Err(format!("Invalid name: {}", str)),
            },
        }
    }
}

impl From<Fqdn> for RData {
    fn from(name: Fqdn) -> Self {
        RData::Cname(name)
    }
}

impl From<IpAddr> for RData {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => ip.into(),
            IpAddr::V6(ip) => ip.into(),
        }
    }
}

impl From<Ipv6Addr> for RData {
    fn from(ip: Ipv6Addr) -> Self {
        RData::Aaaa(ip)
    }
}

impl From<Ipv4Addr> for RData {
    fn from(ip: Ipv4Addr) -> Self {
        RData::A(ip)
    }
}

impl From<String> for RData {
    fn from(str: String) -> Self {
        match IpAddr::from_str(&str) {
            Ok(ip) => ip.into(),
            Err(_) => RData::Cname(str.into()),
        }
    }
}

impl From<&String> for RData {
    fn from(str: &String) -> Self {
        match IpAddr::from_str(str) {
            Ok(ip) => ip.into(),
            Err(_) => RData::Cname(str.into()),
        }
    }
}

impl From<&str> for RData {
    fn from(str: &str) -> Self {
        match IpAddr::from_str(str) {
            Ok(ip) => ip.into(),
            Err(_) => RData::Cname(str.into()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(from = "String")]
pub enum Fqdn {
    Valid(Name),
    Invalid(String),
}

impl Fqdn {
    pub fn is_valid(&self) -> bool {
        matches!(self, Fqdn::Valid(_))
    }

    pub fn child<S>(&self, host: S) -> Self
    where
        S: AsRef<str>,
    {
        match self {
            Fqdn::Valid(name) => match Name::parse(host.as_ref(), Some(name)) {
                Ok(name) => name.into(),
                Err(e) => {
                    let full = format!("{}.{}", host.as_ref(), name);
                    log::warn!("Name {} is an invalid domain name: {}", full, e);
                    Fqdn::Invalid(full)
                }
            },
            Fqdn::Invalid(domain) => Fqdn::Invalid(format!("{}.{}", host.as_ref(), domain)),
        }
    }

    pub fn is_parent(&self, other: &Fqdn) -> bool {
        match (self, other) {
            (Fqdn::Valid(s), Fqdn::Valid(o)) => s.zone_of(o),
            (_, Fqdn::Valid(_)) => {
                // If the child is valid then we would have to be valid.
                false
            }
            _ => {
                let domain = format!(".{}", self);
                let child = other.to_string();
                child.ends_with(&domain)
            }
        }
    }

    pub fn is_sibling(&self, other: &Fqdn) -> bool {
        self.domain().map(|d| d.is_parent(other)).unwrap_or(false)
    }

    pub fn name(&self) -> Option<&Name> {
        match self {
            Fqdn::Valid(name) => Some(name),
            _ => None,
        }
    }

    pub fn domain(&self) -> Option<Self> {
        match self {
            Fqdn::Valid(name) => {
                if name.is_root() {
                    None
                } else {
                    Some(Fqdn::Valid(name.trim_to(name.num_labels() as usize - 1)))
                }
            }
            Fqdn::Invalid(name) => name.find('.').map(|pos| Fqdn::from(&name[pos..])),
        }
    }
}

impl Display for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fqdn::Valid(name) => f.pad(&name.to_string()),
            Fqdn::Invalid(str) => f.pad(str),
        }
    }
}

impl From<&LowerName> for Fqdn {
    fn from(name: &LowerName) -> Self {
        assert!(name.is_fqdn());
        Fqdn::Valid(Name::from(name))
    }
}

impl From<Name> for Fqdn {
    fn from(name: Name) -> Self {
        assert!(name.is_fqdn());
        Fqdn::Valid(name)
    }
}

impl From<String> for Fqdn {
    fn from(str: String) -> Self {
        match Name::parse(&str, None) {
            Ok(mut name) => {
                name.set_fqdn(true);
                name.into()
            }
            Err(e) => {
                log::warn!("Name {} is an invalid domain name: {}", str, e);
                Fqdn::Invalid(str)
            }
        }
    }
}

impl From<&String> for Fqdn {
    fn from(str: &String) -> Self {
        match Name::parse(str.as_str(), None) {
            Ok(mut name) => {
                name.set_fqdn(true);
                Fqdn::Valid(name)
            }
            Err(e) => {
                log::warn!("Name {} is an invalid domain name: {}", str, e);
                Fqdn::Invalid(str.clone())
            }
        }
    }
}

impl From<&str> for Fqdn {
    fn from(str: &str) -> Self {
        match Name::parse(str, None) {
            Ok(mut name) => {
                name.set_fqdn(true);
                Fqdn::Valid(name)
            }
            Err(e) => {
                log::warn!("Name {} is an invalid domain name: {}", str, e);
                Fqdn::Invalid(str.into())
            }
        }
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Clone, Deserialize)]
pub struct Record {
    name: Fqdn,
    pub ttl: Option<u32>,
    rdata: RData,
}

impl Record {
    pub fn new(name: Fqdn, rdata: RData) -> Self {
        if let RData::Cname(ref alias) = rdata {
            if &name == alias {
                panic!("Attempted to create a CNAME cycle with {}", name);
            }
        }

        Self {
            name,
            rdata,
            ttl: None,
        }
    }

    pub fn name(&self) -> &Fqdn {
        &self.name
    }

    pub fn rdata(&self) -> &RData {
        &self.rdata
    }

    pub fn is_valid(&self) -> bool {
        self.name.is_valid() && self.rdata.is_valid()
    }

    pub fn raw(&self, config: &ZoneConfig) -> Option<rr::Record> {
        let name = self.name().name()?;
        let data: rr::RData = self.rdata.clone().try_into().ok()?;

        Some(rr::Record::from_rdata(
            name.clone(),
            self.ttl.unwrap_or(config.ttl),
            data,
        ))
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct RecordSet {
    records: HashMap<Fqdn, HashSet<Record>>,
}

impl RecordSet {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn names(&self) -> Keys<Fqdn, HashSet<Record>> {
        self.records.keys()
    }

    pub fn records(&self) -> Flatten<Values<Fqdn, HashSet<Record>>> {
        self.records.values().flatten()
    }

    pub fn append(&mut self, records: RecordSet) {
        for (name, records) in records.records {
            let our_records = upsert(&mut self.records, &name);
            our_records.extend(records.into_iter().filter(|r| r.is_valid()));
        }
    }

    pub fn insert(&mut self, record: Record) {
        if record.is_valid() {
            let records = upsert(&mut self.records, record.name());
            records.insert(record);
        }
    }

    pub fn len(&self) -> usize {
        let mut count: usize = 0;
        for records in self.records.values() {
            count += records.len()
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn lookup(
        &self,
        name: &Fqdn,
        dns_class: &DNSClass,
        query_type: &RecordType,
    ) -> impl Iterator<Item = Record> {
        let records = if dns_class == &DNSClass::IN {
            match self.records.get(name) {
                Some(records) => records
                    .clone()
                    .drain()
                    .filter(|record| {
                        let record_type = record.rdata().data_type();
                        query_type == &record_type || record_type == RecordType::CNAME
                    })
                    .collect(),
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        records.into_iter()
    }
}

impl IntoIterator for RecordSet {
    type Item = Record;

    type IntoIter = Flatten<IntoValues<Fqdn, HashSet<Record>>>;

    fn into_iter(self) -> Flatten<IntoValues<Fqdn, HashSet<Record>>> {
        self.records.into_values().flatten()
    }
}

impl FromIterator<Record> for RecordSet {
    fn from_iter<T: IntoIterator<Item = Record>>(iter: T) -> Self {
        let mut records = RecordSet::new();
        records.extend(iter);
        records
    }
}

impl FromIterator<RecordSet> for RecordSet {
    fn from_iter<T: IntoIterator<Item = RecordSet>>(iter: T) -> Self {
        let mut records = RecordSet::new();
        records.extend(iter);
        records
    }
}

impl Extend<Record> for RecordSet {
    fn extend<T: IntoIterator<Item = Record>>(&mut self, iter: T) {
        for record in iter {
            self.insert(record);
        }
    }
}

impl Extend<RecordSet> for RecordSet {
    fn extend<T: IntoIterator<Item = RecordSet>>(&mut self, iter: T) {
        for records in iter {
            self.append(records);
        }
    }
}
