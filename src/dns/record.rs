use std::{
    collections::{hash_map::IntoValues, HashMap, HashSet},
    fmt::{self},
    hash::Hash,
    iter::{empty, once, Flatten},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    ops::Deref,
    str::FromStr,
};

use anyhow::{anyhow, Error};
use hickory_server::proto::{
    error::ProtoError,
    rr::{self, rdata, DNSClass, IntoName, Name, RecordType},
};
use serde::{Deserialize, Serialize};

use crate::config::ZoneConfig;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "type", content = "value", rename_all = "UPPERCASE")]
pub(crate) enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Cname(Fqdn),
    Aname(Fqdn),
    Ptr(Fqdn),
}

impl RData {
    pub(crate) fn matches(&self, record_type: RecordType) -> bool {
        match self {
            RData::Cname(_) => true,
            RData::Aname(_) => matches!(record_type, RecordType::A | RecordType::AAAA),
            RData::A(_) => record_type == RecordType::A,
            RData::Aaaa(_) => record_type == RecordType::AAAA,
            RData::Ptr(_) => record_type == RecordType::PTR,
        }
    }
}

impl TryInto<rr::RData> for RData {
    type Error = Error;

    fn try_into(self) -> Result<rr::RData, Self::Error> {
        match self {
            RData::A(ip) => Ok(rr::RData::A(ip.into())),
            RData::Aaaa(ip) => Ok(rr::RData::AAAA(ip.into())),
            RData::Cname(name) => Ok(rr::RData::CNAME(rdata::CNAME(name.into()))),
            RData::Ptr(name) => Ok(rr::RData::PTR(rdata::PTR(name.into()))),
            RData::Aname(_) => Err(anyhow!(
                "ANAME records cannot be converted to DNS responses"
            )),
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

impl TryFrom<&str> for RData {
    type Error = ProtoError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match IpAddr::from_str(s) {
            Ok(ip) => Ok(ip.into()),
            Err(_) => {
                let fqdn = Fqdn::try_from(s)?;
                Ok(RData::Aname(fqdn))
            }
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub(crate) struct Fqdn {
    name: Name,
}

impl Fqdn {
    pub(crate) fn name(&self) -> Name {
        self.name.clone()
    }

    pub(crate) fn child<I>(&self, host: I) -> Result<Self, ProtoError>
    where
        I: IntoName,
    {
        let host = host.into_name()?;
        Ok(host.append_domain(self)?.into())
    }
}

impl fmt::Debug for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.name, f)
    }
}

impl fmt::Display for Fqdn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.name, f)
    }
}

impl From<Fqdn> for Name {
    fn from(fqdn: Fqdn) -> Name {
        fqdn.name
    }
}

impl Deref for Fqdn {
    type Target = Name;

    fn deref(&self) -> &Self::Target {
        &self.name
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
        Fqdn { name }
    }
}

impl TryFrom<&str> for Fqdn {
    type Error = ProtoError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let mut name = Name::from_str(s)?;
        name.set_fqdn(true);
        Ok(name.into())
    }
}

impl TryFrom<String> for Fqdn {
    type Error = ProtoError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_from(s.as_str())
    }
}

#[derive(PartialEq, Hash, Eq, Clone, Deserialize, Serialize)]
pub(crate) struct Record {
    name: Fqdn,
    pub(crate) ttl: Option<u32>,
    rdata: RData,
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ttl = if let Some(ttl) = self.ttl {
            ttl.to_string()
        } else {
            "no expiry".to_string()
        };

        write!(f, "{} -> {:?} ({})", self.name, self.rdata, ttl)
    }
}

impl Record {
    pub(crate) fn new(name: Fqdn, rdata: RData) -> Self {
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

    pub(crate) fn name(&self) -> &Fqdn {
        &self.name
    }

    pub(crate) fn rdata(&self) -> &RData {
        &self.rdata
    }

    pub(crate) fn raw(&self, config: &ZoneConfig) -> Option<rr::Record> {
        let name = self.name().name();
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
pub(crate) struct RecordSet {
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
    pub(crate) fn new() -> Self {
        Default::default()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, name: &Fqdn, rdata: &RData) -> bool {
        self.records
            .get(name)
            .map(|records| records.iter().any(|r| r.rdata == *rdata))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn contains_reverse<I: Into<IpAddr>>(&self, ip: I, name: &Fqdn) -> bool {
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

    #[cfg(test)]
    pub(crate) fn has_name(&self, name: &Name) -> bool {
        self.names.contains(name)
    }

    pub(crate) fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values().flatten()
    }

    fn apply_records<T>(&mut self, fqdn: &Fqdn, records: T)
    where
        T: Iterator<Item = Record>,
    {
        let mut name = fqdn.name();
        self.names.insert(name.clone());
        while name.num_labels() > 1 {
            name = name.trim_to(name.num_labels() as usize - 1);
            self.names.insert(fqdn.name());
        }

        let inner = self.records.entry(fqdn.clone()).or_default();
        for record in records {
            assert_eq!(record.name(), fqdn);

            if !inner.contains(&record) {
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

    pub(crate) fn append(&mut self, records: RecordSet) {
        for (name, records) in records.records {
            self.apply_records(&name, records.into_iter());
        }
    }

    pub(crate) fn insert(&mut self, record: Record) {
        self.apply_records(&record.name().clone(), once(record));
    }

    pub(crate) fn len(&self) -> usize {
        let mut count: usize = 0;
        for records in self.records.values() {
            count += records.len()
        }
        count
    }

    pub(crate) fn is_empty(&self) -> bool {
        for records in self.records.values() {
            if !records.is_empty() {
                return false;
            }
        }

        true
    }

    pub(crate) fn lookup(
        &self,
        name: &Name,
        dns_class: DNSClass,
        query_type: RecordType,
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
                        .filter(move |record| record.rdata().matches(query_type))
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

    #[tracing_test::traced_test]
    #[test]
    fn fqdn() {
        assert_eq!(
            Fqdn::try_from("test.example.com.").unwrap(),
            Fqdn::try_from("test.example.com").unwrap()
        );
    }
}
