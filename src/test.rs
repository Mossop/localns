use std::{ops::Deref, str::FromStr, sync::Arc, time::Duration};

use anyhow::Error;
use hickory_server::proto::rr::domain::Name;
use tempfile::NamedTempFile;
use testcontainers::{
    core::{Mount, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tokio::{
    fs,
    io::AsyncWriteExt,
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

pub(crate) fn name(n: &str) -> Name {
    Name::from_str(n).unwrap()
}

pub(crate) async fn coredns_container(
    zone: &str,
    zonefile: &str,
) -> Result<ContainerAsync<GenericImage>, Error> {
    let zone_file = NamedTempFile::new()?;
    let config_file = NamedTempFile::new()?;

    let mut file = fs::File::from_std(config_file.reopen()?);
    file.write_all(format!("{zone} {{\n  file /zone\n}}\n").as_bytes())
        .await?;

    let mut file = fs::File::from_std(zone_file.reopen()?);
    file.write_all(zonefile.as_bytes()).await?;

    Ok(GenericImage::new("coredns/coredns", "1.11.3")
        .with_wait_for(WaitFor::message_on_stdout("CoreDNS-1.11.3"))
        .with_cmd(["-conf", "/Corefile"])
        .with_mount(Mount::bind_mount(
            zone_file.path().to_str().unwrap(),
            "/zone",
        ))
        .with_mount(Mount::bind_mount(
            config_file.path().to_str().unwrap(),
            "/Corefile",
        ))
        .start()
        .await?)
}
