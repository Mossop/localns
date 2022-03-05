use std::{
    collections::HashSet,
    fmt::Display,
    io,
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
    str::FromStr,
};

use chrono::{TimeZone, Utc};

use crate::config::Upstream;

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub struct AbsoluteName {
    parts: Vec<String>,
}

impl AbsoluteName {
    pub fn new(name: &str) -> Self {
        AbsoluteName {
            parts: name
                .trim_end_matches('.')
                .split('.')
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

    pub fn non_absolute(&self) -> String {
        self.parts.join(".")
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

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub struct RelativeName {
    pub parts: Vec<String>,
}

impl RelativeName {
    pub fn new(name: &str) -> Self {
        RelativeName {
            parts: name
                .trim_end_matches('.')
                .split('.')
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

#[derive(PartialEq, Hash, Eq, Clone, Debug)]
pub enum Address {
    Name(AbsoluteName),
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

impl From<AbsoluteName> for Address {
    fn from(name: AbsoluteName) -> Self {
        Address::Name(name)
    }
}

impl From<&AbsoluteName> for Address {
    fn from(name: &AbsoluteName) -> Self {
        Address::Name(name.clone())
    }
}

impl From<Ipv4Addr> for Address {
    fn from(ip: Ipv4Addr) -> Self {
        Address::V4(ip)
    }
}

impl From<&Ipv4Addr> for Address {
    fn from(ip: &Ipv4Addr) -> Self {
        Address::V4(*ip)
    }
}

impl From<Ipv6Addr> for Address {
    fn from(ip: Ipv6Addr) -> Self {
        Address::V6(ip)
    }
}

impl From<&Ipv6Addr> for Address {
    fn from(ip: &Ipv6Addr) -> Self {
        Address::V6(*ip)
    }
}

impl From<IpAddr> for Address {
    fn from(ip: IpAddr) -> Self {
        Self::from(&ip)
    }
}

impl From<&IpAddr> for Address {
    fn from(ip: &IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => Self::from(ip),
            IpAddr::V6(ip) => Self::from(ip),
        }
    }
}

impl From<String> for Address {
    fn from(name: String) -> Self {
        Self::from(name.as_str())
    }
}

impl From<&String> for Address {
    fn from(name: &String) -> Self {
        Self::from(name.as_str())
    }
}

impl From<&str> for Address {
    fn from(name: &str) -> Self {
        match IpAddr::from_str(name) {
            Ok(ip) => Self::from(ip),
            Err(_) => AbsoluteName::new(name).into(),
        }
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Name(name) => f.pad(&name.non_absolute()),
            Address::V4(ip) => f.pad(&ip.to_string()),
            Address::V6(ip) => f.pad(&ip.to_string()),
        }
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
    Ns(Address),
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
            RecordData::Ns(name) => {
                format!("NS      {}", name)
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

impl From<IpAddr> for RecordData {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => ip.into(),
            IpAddr::V6(ip) => ip.into(),
        }
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
    hostmaster: String,
    refresh_time: u32,
    retry_time: u32,
    expire_time: u32,
    min_ttl: u32,

    upstream: Option<Upstream>,

    records: Vec<ResourceRecord>,
}

impl Zone {
    pub fn new(domain: AbsoluteName, local_ip: &IpAddr, upstream: Option<Upstream>) -> Self {
        Zone {
            hostmaster: format!("hostmaster.{}", domain),
            domain,
            refresh_time: 300,
            retry_time: 300,
            expire_time: 300,
            min_ttl: 300,

            upstream,

            records: vec![ResourceRecord {
                name: Some(RelativeName::new("ns")),
                class: Class::In,
                ttl: 300,
                data: (*local_ip).into(),
            }],
        }
    }

    pub fn add_record(&mut self, record: ResourceRecord) {
        self.records.push(record);
    }

    pub fn write_core(&self, zone_file: &Path, writer: &mut dyn io::Write) -> io::Result<()> {
        let mut buffer = Vec::new();
        let buf = &mut buffer;

        writeln!(buf, "{} {{", self.domain.non_absolute())?;

        match self.upstream {
            Some(ref upstream) => {
                writeln!(buf, "  file {} {{", zone_file.display())?;
                writeln!(buf, "    fallthrough")?;
                writeln!(buf, "  }}")?;
                writeln!(buf)?;
                upstream.write(buf)?;
            }
            None => writeln!(buf, "  file {}zone", self.domain)?,
        }
        writeln!(buf, "}}")?;

        writer.write_all(&buffer)
    }

    pub fn write_zone(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        let epoch = Utc.ymd(2000, 1, 1).and_hms(0, 0, 0);
        let now = Utc::now();
        let serial = now - epoch;

        let mut buffer = Vec::new();
        let buf = &mut buffer;

        writeln!(buf, "$ORIGIN {}", self.domain)?;
        writeln!(
            buf,
            "@ 300 SOA ns.{} ({} {} {} {} {} {})",
            self.domain,
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

        for record in &self.records {
            writeln!(
                buf,
                "{:width$}  {:4} {:4} {}",
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
pub struct Record {
    pub name: AbsoluteName,
    pub ttl: Option<u32>,
    pub data: RecordData,
}

pub type RecordSet = HashSet<Record>;
