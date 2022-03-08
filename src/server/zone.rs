use std::{collections::HashMap, sync::Arc};

use trust_dns_server::{
    authority::{
        AuthLookup, Authority, LookupError, LookupOptions, LookupRecords, MessageRequest,
        UpdateResult, ZoneType,
    },
    client::{
        op::{Query, ResponseCode},
        rr::{self, rdata, LowerName, RecordSet, RecordType},
    },
    server::RequestInfo,
    store::in_memory::InMemoryAuthority,
};

use crate::{
    record::{Name, RData, Record},
    upstream::{Upstream, UpstreamResponse},
};

fn records_to_set(
    records: Vec<rr::Record>,
    lookup_options: LookupOptions,
) -> Option<LookupRecords> {
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

fn lookup_from_response(
    response: UpstreamResponse,
    lookup_options: LookupOptions,
) -> Option<AuthLookup> {
    records_to_set(response.answers, lookup_options).map(|answers| AuthLookup::Records {
        answers,
        additionals: records_to_set(response.additionals, lookup_options),
    })
}

pub struct Zone {
    inner: InMemoryAuthority,
    upstream: Option<Upstream>,
    default_ttl: u32,
    serial: u32,
}

impl Zone {
    pub fn new(origin: Name, upstream: Option<Upstream>) -> Self {
        let inner = InMemoryAuthority::empty(origin.clone(), ZoneType::Primary, false);

        let mut this = Self {
            inner,
            upstream,
            default_ttl: 300,
            serial: 1,
        };

        let soa = rdata::SOA::new(
            Name::parse("ns", Some(&origin)).unwrap(),
            Name::parse("hostmaster", Some(&origin)).unwrap(),
            1,
            300,
            300,
            1800,
            60,
        );

        this.insert(Record::new(origin, RData::SOA(soa)));
        this
    }

    pub fn insert(&mut self, record: Record) {
        let rr = rr::Record::from_rdata(
            record.name,
            record.ttl.unwrap_or(self.default_ttl),
            record.data,
        );

        self.serial += 1;
        self.inner.upsert_mut(rr, self.serial);
    }

    async fn upstream(&self, query: Query, lookup_options: LookupOptions) -> Option<AuthLookup> {
        if let Some(ref upstream) = self.upstream {
            upstream
                .lookup(query)
                .await
                .and_then(|r| lookup_from_response(r, lookup_options))
        } else {
            None
        }
    }
}

#[async_trait::async_trait]
impl Authority for Zone {
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
                    self.upstream(query, lookup_options).await.ok_or(error)
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
                    self.upstream(query, lookup_options).await.ok_or(error)
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
