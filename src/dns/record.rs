use std::{
    collections::{
        hash_map::{IntoValues, Values},
        HashMap, HashSet,
    },
    fmt::{self},
    hash::Hash,
    iter::{empty, once, Flatten},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use hickory_server::proto::rr::{self, rdata, DNSClass, Name, RecordType};
use serde::{Deserialize, Serialize};

use crate::config::ZoneConfig;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Hash)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Cname(Fqdn),
    Ptr(Fqdn),
}

impl RData {
    pub fn data_type(&self) -> RecordType {
        match self {
            RData::A(_) => RecordType::A,
            RData::Aaaa(_) => RecordType::AAAA,
            RData::Cname(_) => RecordType::CNAME,
            RData::Ptr(_) => RecordType::PTR,
        }
    }

    pub fn is_valid(&self) -> bool {
        match self {
            RData::Cname(name) | RData::Ptr(name) => name.is_valid(),
            _ => true,
        }
    }
}

impl TryInto<rr::RData> for RData {
    type Error = String;

    fn try_into(self) -> Result<rr::RData, Self::Error> {
        match self {
            RData::A(ip) => Ok(rr::RData::A(ip.into())),
            RData::Aaaa(ip) => Ok(rr::RData::AAAA(ip.into())),
            RData::Cname(name) => match name {
                Fqdn::Valid(name) => Ok(rr::RData::CNAME(rdata::CNAME(name))),
                Fqdn::Invalid(str) => Err(format!("Invalid name: {}", str)),
            },
            RData::Ptr(name) => match name {
                Fqdn::Valid(name) => Ok(rr::RData::PTR(rdata::PTR(name))),
                Fqdn::Invalid(str) => Err(format!("Invalid name: {}", str)),
            },
        }
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
        Self::from(str.as_str())
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

#[derive(Deserialize, Eq, PartialEq, Debug, Clone)]
#[serde(untagged)]
pub enum RDataConfig {
    Simple(String),
    RData(RData),
}

impl From<RDataConfig> for RData {
    fn from(config: RDataConfig) -> RData {
        match config {
            RDataConfig::Simple(str) => str.into(),
            RDataConfig::RData(rdata) => rdata,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(from = "String")]
#[serde(into = "String")]
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
                    tracing::warn!(name = full, error = %e, "Invalid domain name");
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

impl fmt::Debug for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fqdn::Valid(n) => write!(f, "{n}"),
            Fqdn::Invalid(s) => write!(f, "INVALID({s})"),
        }
    }
}

impl fmt::Display for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fqdn::Valid(name) => f.pad(&name.to_string()),
            Fqdn::Invalid(str) => f.pad(str),
        }
    }
}

impl From<Fqdn> for String {
    fn from(fqdn: Fqdn) -> String {
        fqdn.to_string()
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
                tracing::warn!(name = str, error = %e, "Invalid domain name");
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
                tracing::warn!(name = str, error = %e, "Invalid domain name");
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
                tracing::warn!(name = str, error = %e, "Invalid domain name");
                Fqdn::Invalid(str.into())
            }
        }
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Clone)]
pub enum RecordSource {
    Local,
    Remote,
}

fn remote_source() -> RecordSource {
    RecordSource::Remote
}

#[derive(PartialEq, Hash, Eq, Clone, Deserialize, Serialize)]
pub struct Record {
    name: Fqdn,
    #[serde(skip)]
    #[serde(default = "remote_source")]
    pub source: RecordSource,
    pub ttl: Option<u32>,
    rdata: RData,
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ttl = if let Some(ttl) = self.ttl {
            ttl.to_string()
        } else {
            "no expiry".to_string()
        };

        write!(
            f,
            "{} -> {:?} ({:?}, {})",
            self.name, self.rdata, self.source, ttl
        )
    }
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
            source: RecordSource::Local,
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

    pub(crate) fn raw(&self, config: &ZoneConfig) -> Option<rr::Record> {
        let name = self.name().name()?;
        let data: rr::RData = self.rdata.clone().try_into().ok()?;

        Some(rr::Record::from_rdata(
            name.clone(),
            self.ttl.unwrap_or(config.ttl),
            data,
        ))
    }
}

#[derive(Default, PartialEq, Eq, Clone, Deserialize, Serialize)]
#[serde(from = "Vec<Record>")]
#[serde(into = "Vec<Record>")]
pub struct RecordSet {
    records: HashMap<Fqdn, HashSet<Record>>,
    reverse: HashMap<IpAddr, Record>,
    names: HashSet<Name>,
}

impl fmt::Debug for RecordSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let records: Vec<&Record> = self.records().collect();
        let reverse: HashMap<&IpAddr, String> = self
            .reverse
            .iter()
            .map(|(ip, record)| {
                if let RData::Ptr(p) = &record.rdata {
                    (ip, p.to_string())
                } else {
                    unreachable!()
                }
            })
            .collect();

        f.debug_struct("RecordSet")
            .field("records", &records)
            .field("reverse", &reverse)
            .finish()
    }
}

impl RecordSet {
    pub fn new() -> Self {
        Default::default()
    }

    #[cfg(test)]
    pub fn contains(&self, name: &Fqdn, rdata: &RData) -> bool {
        self.records
            .get(name)
            .map(|records| records.iter().any(|r| r.rdata == *rdata))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn contains_reverse<I: Into<IpAddr>>(&self, ip: I, name: &Fqdn) -> bool {
        self.reverse
            .get(&ip.into())
            .map(|r| {
                if let RData::Ptr(p) = &r.rdata {
                    p == name
                } else {
                    false
                }
            })
            .unwrap_or_default()
    }

    pub fn has_name(&self, name: &Name) -> bool {
        self.names.contains(name)
    }

    pub fn records(&self) -> Flatten<Values<Fqdn, HashSet<Record>>> {
        self.records.values().flatten()
    }

    fn apply_records<T>(&mut self, name: &Fqdn, records: T)
    where
        T: Iterator<Item = Record>,
    {
        if let Some(mut name) = name.name().cloned() {
            self.names.insert(name.clone());
            while name.num_labels() > 1 {
                name = name.trim_to(name.num_labels() as usize - 1);
                self.names.insert(name.clone());
            }
        } else {
            // Invalid name, skip.
            return;
        }

        let inner = self.records.entry(name.clone()).or_default();
        for record in records {
            assert_eq!(record.name(), name);

            if record.is_valid() && !inner.contains(&record) {
                match record.rdata() {
                    RData::A(ip) => {
                        let mut ptr =
                            Record::new(Name::from(*ip).into(), RData::Ptr(record.name().clone()));
                        ptr.ttl = record.ttl;
                        self.reverse.insert(IpAddr::from(*ip), ptr);
                    }
                    RData::Aaaa(ip) => {
                        let mut ptr =
                            Record::new(Name::from(*ip).into(), RData::Ptr(record.name().clone()));
                        ptr.ttl = record.ttl;
                        self.reverse.insert(IpAddr::from(*ip), ptr);
                    }
                    _ => {}
                }

                inner.insert(record);
            }
        }
    }

    pub fn append(&mut self, records: RecordSet) {
        for (name, records) in records.records {
            self.apply_records(&name, records.into_iter());
        }
    }

    pub fn insert(&mut self, record: Record) {
        self.apply_records(&record.name().clone(), once(record));
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
        name: &Name,
        dns_class: DNSClass,
        query_type: RecordType,
        recurse: bool,
    ) -> Box<dyn Iterator<Item = Record> + '_> {
        if dns_class != DNSClass::IN {
            return Box::new(empty());
        }

        match query_type {
            RecordType::PTR => Box::new(
                name.parse_arpa_name()
                    .ok()
                    .and_then(|net| self.reverse.get(&net.addr()))
                    .cloned()
                    .into_iter(),
            ),
            _ => match self.records.get(&name.clone().into()) {
                Some(records) => Box::new(
                    records
                        .iter()
                        .filter(move |record| {
                            let record_type = record.rdata().data_type();
                            query_type == record_type
                                || (record_type == RecordType::CNAME && recurse)
                        })
                        .cloned(),
                ),
                None => Box::new(empty()),
            },
        }
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

impl From<Vec<Record>> for RecordSet {
    fn from(records: Vec<Record>) -> Self {
        Self::from_iter(records)
    }
}

impl From<RecordSet> for Vec<Record> {
    fn from(records: RecordSet) -> Vec<Record> {
        records.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::dns::Fqdn;

    #[test]
    fn fqdn() {
        assert_eq!(
            Fqdn::from("test.example.com."),
            Fqdn::from("test.example.com")
        );
    }
}
