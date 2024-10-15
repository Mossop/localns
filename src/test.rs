use std::{ops::Deref, sync::Arc, time::Duration};

use tokio::{
    sync::{Mutex, Notify},
    time::timeout,
};

use crate::{
    dns::RecordSet,
    sources::{SourceId, SourceRecords},
    RecordServer,
};

#[derive(Clone)]
pub(crate) struct TestServer {
    source_id: SourceId,
    notify: Arc<Notify>,
    records: Arc<Mutex<Option<RecordSet>>>,
}

impl TestServer {
    pub(crate) fn new(source_id: &SourceId) -> Self {
        Self {
            source_id: source_id.clone(),
            notify: Arc::new(Notify::new()),
            records: Default::default(),
        }
    }

    pub(crate) async fn wait_for_change(&self) {
        if timeout(Duration::from_secs(1), self.notify.notified())
            .await
            .is_err()
        {
            panic!("Timed out waiting for new records");
        }
    }

    pub(crate) async fn wait_for_records<F>(&self, mut cb: F) -> RecordSet
    where
        F: FnMut(&RecordSet) -> bool,
    {
        loop {
            let notified = {
                let inner = self.records.lock().await;
                if let Some(records) = inner.deref() {
                    if cb(records) {
                        return records.clone();
                    }
                }

                self.wait_for_change()
            };

            notified.await;
        }
    }
}

impl RecordServer for TestServer {
    async fn add_source_records(&self, new_records: SourceRecords) {
        assert_eq!(new_records.source_id, self.source_id);

        let mut inner = self.records.lock().await;
        self.notify.notify_waiters();
        inner.replace(new_records.records);
    }

    async fn clear_source_records(&self, source_id: &SourceId) {
        assert_eq!(source_id, &self.source_id);

        let mut inner = self.records.lock().await;
        self.notify.notify_waiters();
        inner.take();
    }
}
