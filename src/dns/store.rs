use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::Arc,
};

use tokio::sync::{
    watch::{channel, Receiver, Sender},
    RwLock,
};

use crate::{
    dns::RecordSet,
    sources::{IntoSourceRecordSet, SourceId, SourceRecords},
};

#[derive(Clone)]
pub(crate) struct RecordStore {
    pub(crate) source_records: Arc<RwLock<HashMap<SourceId, Vec<SourceRecords>>>>,
    pub(crate) sender: Sender<RecordSet>,
}

impl RecordStore {
    pub(crate) fn new() -> Self {
        let (sender, _) = channel(RecordSet::new());

        Self {
            source_records: Default::default(),
            sender,
        }
    }

    pub(crate) fn receiver(&self) -> Receiver<RecordSet> {
        self.sender.subscribe()
    }

    fn dedupe_sources<G>(source_records: &G) -> impl Iterator<Item = &'_ SourceRecords>
    where
        G: Deref<Target = HashMap<SourceId, Vec<SourceRecords>>>,
    {
        let mut joined: HashMap<SourceId, &SourceRecords> = HashMap::new();

        for source_records in source_records.values().flatten() {
            joined
                .entry(source_records.source_id.clone())
                .and_modify(|existing| {
                    if existing.timestamp < source_records.timestamp {
                        *existing = source_records
                    }
                })
                .or_insert_with(|| source_records);
        }

        joined.into_values()
    }

    fn update_record_set<G>(&self, source_records: &G)
    where
        G: Deref<Target = HashMap<SourceId, Vec<SourceRecords>>>,
    {
        let records = Self::dedupe_sources(source_records)
            .map(|sr| &sr.records)
            .cloned()
            .collect();

        self.sender.send_replace(records);
    }

    pub(crate) async fn resolve_source_records(&self) -> Vec<SourceRecords> {
        let source_records = self.source_records.read().await;

        Self::dedupe_sources(&source_records).cloned().collect()
    }

    pub(crate) async fn add_source_records<I: IntoSourceRecordSet>(
        &self,
        source_id: &SourceId,
        new_records: I,
    ) {
        let new_records = new_records.into_source_record_set(source_id);

        let mut source_records = self.source_records.write().await;
        source_records.insert(source_id.clone(), new_records);
        self.update_record_set(&source_records);
    }

    pub(crate) async fn clear_source_records(&self, source_id: &SourceId) {
        let mut source_records = self.source_records.write().await;
        source_records.remove(source_id);
        self.update_record_set(&source_records);
    }

    pub(crate) async fn prune_sources(&self, keep: &HashSet<SourceId>) {
        let mut source_records = self.source_records.write().await;
        source_records.retain(|source_id, _| keep.contains(source_id));
        self.update_record_set(&source_records);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::TempDir;
    use uuid::Uuid;

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

        let source_id_1 = SourceId::new(&Uuid::new_v4(), SourceType::File, "test");

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

        let source_id_2 = SourceId::new(&Uuid::new_v4(), SourceType::Docker, "test");

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
