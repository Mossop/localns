use std::{fmt, net::Ipv4Addr};

#[derive(PartialEq, Eq)]
pub enum Class {
    In,
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("IN")
    }
}

#[derive(PartialEq, Eq)]
pub enum RecordData {
    A(Ipv4Addr),
    Cname(String),
}

impl fmt::Display for RecordData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let str = match self {
            RecordData::A(address) => format!("A       {}", address),
            RecordData::Cname(name) => format!("CNAME   {}", name),
        };

        f.pad(&str)
    }
}

#[derive(PartialEq, Eq)]
pub struct ResourceRecord {
    pub name: String,
    pub class: Class,
    pub ttl: u32,
    pub data: RecordData,
}

impl fmt::Display for ResourceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let str = format!(
            "{:15} {:5} {:3} {}",
            self.name, self.ttl, self.class, self.data
        );

        f.pad(&str)
    }
}
