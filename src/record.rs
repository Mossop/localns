use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
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

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.name.partial_cmp(&other.name) {
            Some(Ordering::Equal) => {}
            ord => return ord,
        }
        self.data.partial_cmp(&other.data)
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.data.cmp(&other.data))
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

pub fn split(records: &RecordSet) -> HashMap<Name, RecordSet> {
    let mut zones: HashMap<Name, RecordSet> = HashMap::new();
    let mut records: Vec<Record> = records.iter().cloned().collect();
    records.sort_by(|r1, r2| r1.name.cmp(&r2.name).reverse());

    for record in records {
        if let Some(records) = zones.get_mut(&record.name) {
            records.insert(record.clone());
        };

        let domain = record.name.trim_to((record.name.num_labels() - 1).into());

        match zones.get_mut(&domain) {
            Some(records) => {
                records.insert(record);
            }
            None => {
                let mut records = RecordSet::new();
                records.insert(record);
                zones.insert(domain, records);
            }
        };
    }

    zones
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use crate::config::Host;

    use super::{fqdn, Name, Record, RecordSet};

    #[test]
    fn split() {
        fn records(mut list: Vec<(&str, &str)>) -> RecordSet {
            list.drain(..)
                .map(|(n, h)| Record::new(fqdn(n), Host::from(h).rdata()))
                .collect()
        }

        fn domains(zones: &HashMap<Name, RecordSet>) -> Vec<String> {
            let mut zone_names: Vec<&Name> = zones.keys().collect();
            zone_names.sort();
            zone_names.iter().map(ToString::to_string).collect()
        }

        fn domain_records(zones: &HashMap<Name, RecordSet>, zone: &str) -> Vec<Record> {
            let mut records: Vec<Record> = Name::from_str(zone)
                .ok()
                .and_then(|n| zones.get(&n))
                .map(|r| r.iter().cloned().collect())
                .unwrap_or_default();
            records.sort();
            records
        }

        fn record_names(records: &[Record]) -> Vec<String> {
            records.iter().map(|r| r.name.to_string()).collect()
        }

        let records: RecordSet = records(vec![
            ("menger.lambkin.oxymoronical.com", "10.10.14.254"),
            ("traefik.cloud.oxymoronical.com", "10.10.16.251"),
            ("cloud.oxymoronical.com", "10.10.240.3"),
        ]);

        let zones = super::split(&records);

        let zone_names = domains(&zones);
        assert_eq!(
            zone_names,
            vec![
                "oxymoronical.com.",
                "cloud.oxymoronical.com.",
                "lambkin.oxymoronical.com."
            ]
        );

        let records = domain_records(&zones, "oxymoronical.com.");
        assert_eq!(record_names(&records), vec!["cloud.oxymoronical.com."]);

        let records = domain_records(&zones, "cloud.oxymoronical.com.");
        assert_eq!(
            record_names(&records),
            vec!["cloud.oxymoronical.com.", "traefik.cloud.oxymoronical.com."]
        );

        let records = domain_records(&zones, "lambkin.oxymoronical.com.");
        assert_eq!(
            record_names(&records),
            vec!["menger.lambkin.oxymoronical.com."]
        );
    }
}
