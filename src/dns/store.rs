use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use ::serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use tokio::sync::{
    watch::{channel, Receiver, Sender},
    RwLock,
};

use crate::{dns::RecordSet, sources::SourceId, ServerId};

pub(crate) type ServerRecords = HashMap<SourceId, RecordSet>;

mod serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) mod server_records {
        use super::*;
        use crate::{
            dns::{store::ServerRecords, RecordSet},
            sources::SourceId,
        };

        #[derive(Serialize)]
        struct SeRepr<'a> {
            #[serde(flatten)]
            source_id: &'a SourceId,
            records: &'a RecordSet,
        }

        #[derive(Deserialize)]
        struct DeRepr {
            #[serde(flatten)]
            source_id: SourceId,
            records: RecordSet,
        }

        pub(crate) fn serialize<S>(
            server_records: &ServerRecords,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let repr: Vec<SeRepr<'_>> = server_records
                .iter()
                .map(|(source_id, records)| SeRepr { source_id, records })
                .collect();

            repr.serialize(serializer)
        }

        pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<ServerRecords, D::Error>
        where
            D: Deserializer<'de>,
        {
            let list = Vec::<DeRepr>::deserialize(deserializer)?;

            Ok(list
                .into_iter()
                .map(|repr| (repr.source_id, repr.records))
                .collect())
        }
    }

    pub(super) mod remotes {
        use std::collections::HashMap;

        use super::*;
        use crate::{dns::store::RemoteServerRecords, ServerId};

        #[derive(Serialize)]
        struct SeRepr<'a> {
            #[serde(with = "uuid::serde::braced")]
            server_id: ServerId,
            #[serde(flatten)]
            rsr: &'a RemoteServerRecords,
        }

        #[derive(Deserialize)]
        struct DeRepr {
            #[serde(with = "uuid::serde::braced")]
            server_id: ServerId,
            #[serde(flatten)]
            rsr: RemoteServerRecords,
        }

        pub(crate) fn serialize<S>(
            remotes: &HashMap<ServerId, RemoteServerRecords>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let repr: Vec<SeRepr<'_>> = remotes
                .iter()
                .map(|(server_id, rsr)| SeRepr {
                    server_id: *server_id,
                    rsr,
                })
                .collect();

            repr.serialize(serializer)
        }

        pub(crate) fn deserialize<'de, D>(
            deserializer: D,
        ) -> Result<HashMap<ServerId, RemoteServerRecords>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let list = Vec::<DeRepr>::deserialize(deserializer)?;

            Ok(list
                .into_iter()
                .map(|repr| (repr.server_id, repr.rsr))
                .collect())
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RemoteServerRecords {
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) expiry: DateTime<Utc>,
    #[serde(with = "serde::server_records")]
    pub(crate) records: ServerRecords,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RecordStoreData {
    #[serde(with = "serde::server_records")]
    pub(crate) local: ServerRecords,
    #[serde(with = "serde::remotes")]
    pub(crate) remote: HashMap<ServerId, RemoteServerRecords>,
}

impl RecordStoreData {
    fn expire_remotes(&mut self) {
        let now = Utc::now();
        self.remote.retain(|_, rsr| rsr.expiry > now);
    }

    fn add_remote_records(&mut self, remotes: HashMap<ServerId, RemoteServerRecords>) {
        for (server_id, rsr) in remotes.into_iter() {
            if let Some(existing) = self.remote.get_mut(&server_id) {
                if existing.timestamp < rsr.timestamp {
                    *existing = rsr;
                } else if existing.timestamp == rsr.timestamp && existing.expiry < rsr.expiry {
                    existing.expiry = rsr.expiry;
                }
            } else {
                self.remote.insert(server_id, rsr);
            }
        }
    }

    fn resolve_records(&self) -> impl Iterator<Item = &'_ RecordSet> {
        let local_records = self.local.values();
        let remote_records = self.remote.values().flat_map(|rsr| rsr.records.values());

        local_records.chain(remote_records)
    }
}

#[derive(Clone)]
pub(crate) struct RecordStore {
    pub(crate) store_data: Arc<RwLock<RecordStoreData>>,
    pub(crate) sender: Sender<RecordSet>,
}

impl RecordStore {
    pub(crate) fn new() -> Self {
        let (sender, _) = channel(RecordSet::new());

        Self {
            store_data: Default::default(),
            sender,
        }
    }

    pub(crate) fn receiver(&self) -> Receiver<RecordSet> {
        self.sender.subscribe()
    }

    fn update_record_set(&self, store_data: &RecordStoreData) {
        let records = store_data.resolve_records().cloned().collect();

        self.sender.send_replace(records);
    }

    pub(crate) async fn store_data(&self) -> RecordStoreData {
        let mut store_data = self.store_data.read().await.clone();

        store_data.expire_remotes();
        store_data
    }

    pub(crate) async fn add_remote_records(&self, remotes: HashMap<ServerId, RemoteServerRecords>) {
        let mut store_data = self.store_data.write().await;
        store_data.add_remote_records(remotes);
        store_data.expire_remotes();
        self.update_record_set(&store_data);
    }

    pub(crate) async fn add_source_records(&self, source_id: &SourceId, new_records: RecordSet) {
        let mut store_data = self.store_data.write().await;
        store_data.local.insert(source_id.clone(), new_records);
        store_data.expire_remotes();
        self.update_record_set(&store_data);
    }

    pub(crate) async fn clear_source_records(&self, source_id: &SourceId) {
        let mut store_data = self.store_data.write().await;
        store_data.local.remove(source_id);
        store_data.expire_remotes();
        self.update_record_set(&store_data);
    }

    pub(crate) async fn prune_sources(&self, keep: &HashSet<SourceId>) {
        let mut store_data = self.store_data.write().await;
        store_data
            .local
            .retain(|source_id, _| keep.contains(source_id));
        store_data.expire_remotes();
        self.update_record_set(&store_data);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        dns::{RData, Record, RecordSet},
        sources::{SourceId, SourceType},
        test::{fqdn, write_file},
    };

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn record_store() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("config.yml");

        write_file(
            &config_file,
            r#"
server:
  port: 53531
"#,
        )
        .await;

        let record_store = RecordStore::new();

        let source_id_1 = SourceId::new(SourceType::File, "test");

        let mut records_1 = RecordSet::new();
        records_1.insert(Record::new(
            fqdn("www.example.org"),
            RData::Cname(fqdn("other.example.org")),
        ));

        record_store
            .add_source_records(&source_id_1, records_1.clone())
            .await;

        let server_records = record_store.records();
        assert_eq!(server_records.len(), 1);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        record_store
            .add_source_records(&source_id_1, records_1.clone())
            .await;

        let server_records = record_store.records();
        assert_eq!(server_records.len(), 1);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        records_1.insert(Record::new(
            fqdn("old.example.org"),
            RData::Cname(fqdn("other.example.org")),
        ));

        record_store
            .add_source_records(&source_id_1, records_1.clone())
            .await;

        let server_records = record_store.records();
        assert_eq!(server_records.len(), 2);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));
        assert!(server_records.contains(
            &fqdn("old.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));

        let source_id_2 = SourceId::new(SourceType::Docker, "test");

        let mut records_2 = RecordSet::new();
        records_2.insert(Record::new(
            fqdn("other.data.com"),
            RData::Cname(fqdn("www.data.com")),
        ));

        record_store
            .add_source_records(&source_id_2, records_2.clone())
            .await;

        let server_records = record_store.records();
        assert_eq!(server_records.len(), 3);
        assert!(server_records.contains(
            &fqdn("www.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));
        assert!(server_records.contains(
            &fqdn("old.example.org"),
            &RData::Cname(fqdn("other.example.org"))
        ));
        assert!(
            server_records.contains(&fqdn("other.data.com"), &RData::Cname(fqdn("www.data.com")))
        );

        let mut keep = HashSet::new();
        keep.insert(source_id_2.clone());
        record_store.prune_sources(&keep).await;

        let server_records = record_store.records();
        assert_eq!(server_records.len(), 1);
        assert!(
            server_records.contains(&fqdn("other.data.com"), &RData::Cname(fqdn("www.data.com")))
        );

        record_store.clear_source_records(&source_id_2).await;

        let server_records = record_store.records();
        assert!(server_records.is_empty());
    }
}
