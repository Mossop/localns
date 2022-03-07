use std::{collections::HashMap, sync::Arc};

use tokio::net::TcpStream;
use trust_dns_server::{
    authority::{
        AuthLookup, Authority, LookupError, LookupOptions, LookupRecords, MessageRequest,
        UpdateResult, ZoneType,
    },
    client::{
        client::{AsyncClient, ClientHandle},
        op::{Query, ResponseCode},
        rr::{rdata, LowerName, Name, RData, Record, RecordSet, RecordType},
        tcp::TcpClientStream,
    },
    proto::iocompat::AsyncIoTokioAsStd,
    server::RequestInfo,
    store::in_memory::InMemoryAuthority,
};

async fn connect_client() -> AsyncClient {
    let address = "10.10.14.253:53".parse().unwrap();
    let (stream, sender) = TcpClientStream::<AsyncIoTokioAsStd<TcpStream>>::new(address);

    let client = AsyncClient::new(stream, sender, None);
    let (client, bg) = client.await.expect("connection failed");
    tokio::spawn(bg);

    client
}

fn records_to_set(records: Vec<Record>, lookup_options: LookupOptions) -> Option<LookupRecords> {
    let mut sets: HashMap<(Name, RecordType), RecordSet> = HashMap::new();

    for record in records {
        match sets.get_mut(&(record.name().clone(), record.record_type())) {
            Some(set) => {
                set.insert(record, 0);
            }
            None => {
                let mut set = RecordSet::new(record.name(), record.record_type(), 0);
                set.insert(record.clone(), 0);
                sets.insert((record.name().clone(), record.record_type()), set);
            }
        }
    }

    let mut records: Vec<Arc<RecordSet>> = sets.into_values().map(Arc::new).collect();
    match records.len() {
        0 => None,
        1 => Some(LookupRecords::Records {
            lookup_options,
            records: records.remove(0),
        }),
        _ => Some(LookupRecords::ManyRecords(lookup_options, records)),
    }
}

async fn lookup(query: Query, lookup_options: LookupOptions) -> Option<AuthLookup> {
    log::debug!("Forwarding query for {} upstream", query.name());
    let mut client = connect_client().await;

    let (answers, additionals) = match client
        .query(
            query.name().clone(),
            query.query_class(),
            query.query_type(),
        )
        .await
    {
        Ok(mut response) => (response.take_answers(), response.take_additionals()),
        Err(e) => {
            log::warn!("Upstream DNS server returned error: {}", e);
            return None;
        }
    };

    records_to_set(answers, lookup_options).map(|answers| AuthLookup::Records {
        answers,
        additionals: records_to_set(additionals, lookup_options),
    })
}

pub struct SemiAuthoritativeAuthority {
    inner: InMemoryAuthority,
    serial: u32,
}

impl SemiAuthoritativeAuthority {
    pub async fn new(origin: Name) -> Self {
        let mut inner = InMemoryAuthority::empty(origin.clone(), ZoneType::Primary, false);

        let soa = rdata::SOA::new(
            Name::parse("ns", Some(&origin)).unwrap(),
            Name::parse("hostmaster", Some(&origin)).unwrap(),
            1,
            300,
            300,
            300,
            300,
        );
        let record = Record::from_rdata(origin, 300, RData::SOA(soa));
        inner.upsert_mut(record, 1);

        Self { inner, serial: 1 }
    }

    pub fn insert(&mut self, record: Record) {
        self.serial += 1;
        self.inner.upsert_mut(record, self.serial);
    }
}

#[async_trait::async_trait]
impl Authority for SemiAuthoritativeAuthority {
    type Lookup = AuthLookup;

    fn zone_type(&self) -> ZoneType {
        self.inner.zone_type()
    }

    fn is_axfr_allowed(&self) -> bool {
        false
    }

    async fn update(&self, _update: &MessageRequest) -> UpdateResult<bool> {
        Err(ResponseCode::NotImp)
    }

    fn origin(&self) -> &LowerName {
        self.inner.origin()
    }

    async fn lookup(
        &self,
        name: &LowerName,
        query_type: RecordType,
        lookup_options: LookupOptions,
    ) -> Result<Self::Lookup, LookupError> {
        match self.inner.lookup(name, query_type, lookup_options).await {
            Ok(lookup) => Ok(lookup),
            Err(error) => {
                if error.is_nx_domain() {
                    let query = Query::query(name.into(), query_type);
                    lookup(query, lookup_options).await.ok_or(error)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn search(
        &self,
        request_info: RequestInfo<'_>,
        lookup_options: LookupOptions,
    ) -> Result<Self::Lookup, LookupError> {
        let query = request_info.query.original().clone();

        match self.inner.search(request_info, lookup_options).await {
            Ok(lookup) => Ok(lookup),
            Err(error) => {
                if error.is_nx_domain() {
                    lookup(query, lookup_options).await.ok_or(error)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn get_nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> Result<Self::Lookup, LookupError> {
        Ok(AuthLookup::default())
    }
}
