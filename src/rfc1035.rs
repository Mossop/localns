use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io,
    io::Write,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use chrono::{TimeZone, Utc};

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub struct AbsoluteName {
    parts: Vec<String>,
}

impl AbsoluteName {
    pub fn new(name: &str) -> Self {
        AbsoluteName {
            parts: name
                .trim_end_matches('.')
                .split(".")
                .map(|s| s.to_owned())
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.parts.iter().fold(0, |sum, part| sum + part.len()) + self.parts.len()
    }

    pub fn prepend(&self, name: RelativeName) -> AbsoluteName {
        AbsoluteName::new(&format!("{}.{}", name, self))
    }

    pub fn split(&self) -> Option<(RelativeName, AbsoluteName)> {
        if self.parts.len() <= 1 {
            None
        } else {
            Some((
                RelativeName::new(&self.parts[0]),
                AbsoluteName {
                    parts: self.parts.clone().drain(1..).collect(),
                },
            ))
        }
    }
}

impl Display for AbsoluteName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&format!("{}.", self.parts.join(".")))
    }
}

impl From<String> for AbsoluteName {
    fn from(name: String) -> Self {
        AbsoluteName::new(name.as_str())
    }
}

impl From<&String> for AbsoluteName {
    fn from(name: &String) -> Self {
        AbsoluteName::new(name.as_str())
    }
}

impl From<&str> for AbsoluteName {
    fn from(name: &str) -> Self {
        AbsoluteName::new(name)
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct RelativeName {
    pub parts: Vec<String>,
}

impl RelativeName {
    pub fn new(name: &str) -> Self {
        RelativeName {
            parts: name
                .trim_end_matches('.')
                .split(".")
                .map(|s| s.to_owned())
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.parts.iter().fold(0, |sum, part| sum + part.len()) + self.parts.len() - 1
    }
}

impl Display for RelativeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&self.parts.join("."))
    }
}

impl From<String> for RelativeName {
    fn from(name: String) -> Self {
        RelativeName::new(name.as_str())
    }
}

impl From<&String> for RelativeName {
    fn from(name: &String) -> Self {
        RelativeName::new(name.as_str())
    }
}

impl From<&str> for RelativeName {
    fn from(name: &str) -> Self {
        RelativeName::new(name)
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum Class {
    In,
}

impl Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("IN")
    }
}

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub enum RecordData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Cname(AbsoluteName),
}

impl RecordData {
    pub fn write(&self) -> String {
        match self {
            RecordData::A(ip) => {
                format!("A       {}", ip)
            }
            RecordData::Aaaa(ip) => {
                format!("AAAA    {}", ip)
            }
            RecordData::Cname(name) => {
                format!("CNAME   {}", name)
            }
        }
    }
}

impl From<Ipv4Addr> for RecordData {
    fn from(ip: Ipv4Addr) -> Self {
        RecordData::A(ip)
    }
}

impl From<Ipv6Addr> for RecordData {
    fn from(ip: Ipv6Addr) -> Self {
        RecordData::Aaaa(ip)
    }
}

impl From<AbsoluteName> for RecordData {
    fn from(name: AbsoluteName) -> Self {
        RecordData::Cname(name)
    }
}

impl From<String> for RecordData {
    fn from(name: String) -> Self {
        RecordData::from(name.as_str())
    }
}

impl From<&str> for RecordData {
    fn from(name: &str) -> Self {
        if let Ok(ip) = Ipv4Addr::from_str(name) {
            RecordData::from(ip)
        } else if let Ok(ip) = Ipv6Addr::from_str(name) {
            RecordData::from(ip)
        } else {
            RecordData::from(AbsoluteName::from(name))
        }
    }
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct ResourceRecord {
    pub name: Option<RelativeName>,
    pub class: Class,
    pub ttl: u32,
    pub data: RecordData,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct Zone {
    pub domain: AbsoluteName,
    pub nameserver: AbsoluteName,
    pub hostmaster: String,
    pub refresh_time: u32,
    pub retry_time: u32,
    pub expire_time: u32,
    pub min_ttl: u32,

    pub records: Vec<ResourceRecord>,
}

impl Zone {
    pub fn new(domain: &AbsoluteName, nameserver: &AbsoluteName) -> Self {
        Zone {
            domain: domain.clone(),
            nameserver: nameserver.clone(),
            hostmaster: format!("hostmaster.{}", domain),
            refresh_time: 300,
            retry_time: 300,
            expire_time: 300,
            min_ttl: 300,

            records: Vec::new(),
        }
    }

    pub fn write(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        let epoch = Utc.ymd(2000, 1, 1).and_hms(0, 0, 0);
        let now = Utc::now();
        let serial = now - epoch;

        let mut buffer = Vec::new();
        writeln!(&mut buffer, "$ORIGIN {}", self.domain)?;
        writeln!(
            &mut buffer,
            "@ 300 SOA {} ({} {} {} {} {} {})",
            self.nameserver,
            self.hostmaster,
            serial.num_minutes() as u32,
            self.refresh_time,
            self.retry_time,
            self.expire_time,
            self.min_ttl
        )?;

        let mut longest: usize = 0;
        for record in &self.records {
            if let Some(name) = &record.name {
                if name.len() > longest {
                    longest = name.len();
                }
            }
        }

        longest += 8 - (longest % 8);

        for record in &self.records {
            writeln!(
                &mut buffer,
                "{:width$} {:7} {:7} {}",
                record
                    .name
                    .as_ref()
                    .map(|n| n.to_string())
                    .as_deref()
                    .unwrap_or(""),
                record.ttl,
                record.class,
                record.data.write(),
                width = longest
            )?;
        }

        writer.write_all(&buffer)
    }
}

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
struct Record {
    pub name: AbsoluteName,
    pub data: RecordData,
}

#[derive(Default, PartialEq, Eq, Clone, Debug)]
pub struct Records {
    records: HashSet<Record>,
}

impl Records {
    pub fn new() -> Records {
        Default::default()
    }

    pub fn add_record(&mut self, name: AbsoluteName, data: RecordData) {
        self.records.insert(Record { name, data });
    }

    pub fn zones(&self, nameserver: &AbsoluteName) -> Vec<Zone> {
        let mut zones: HashMap<AbsoluteName, Zone> = Default::default();

        for record in &self.records {
            if let Some((name, domain)) = record.name.split() {
                let resource = ResourceRecord {
                    name: Some(name.clone()),
                    class: Class::In,
                    ttl: 300,
                    data: record.data.clone(),
                };

                match zones.get_mut(&domain) {
                    Some(zone) => zone.records.push(resource),
                    None => {
                        let mut zone = Zone::new(&domain, nameserver);
                        zone.records.push(resource);
                        zones.insert(domain.clone(), zone);
                    }
                };
            }
        }

        zones.into_values().collect()
    }
}
