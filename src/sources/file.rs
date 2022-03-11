use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
};

use futures::{future::Abortable, StreamExt};
use serde::Deserialize;

use crate::{
    config::Config,
    dns::{Fqdn, RData, Record, RecordSet},
    watcher::{watch, FileEvent},
};

use super::SourceContext;

pub type FileConfig = PathBuf;

#[derive(Deserialize)]
#[serde(untagged)]
enum Lease {
    Simple(RData),
}

type LeaseFile = HashMap<Fqdn, Lease>;

fn parse_file(name: &str, lease_file: &Path) -> Result<RecordSet, String> {
    log::trace!("({}) Parsing lease file {}...", name, lease_file.display());

    let f = File::open(lease_file)
        .map_err(|e| format!("Failed to open file at {}: {}", lease_file.display(), e))?;

    let leases: LeaseFile =
        serde_yaml::from_reader(f).map_err(|e| format!("Failed to parse leases: {}", e))?;

    let mut records = RecordSet::new();

    for (name, lease) in leases {
        let Lease::Simple(rdata) = lease;
        records.insert(Record::new(name, rdata));
    }

    Ok(records)
}

pub(super) fn source(
    name: String,
    config: Config,
    file_config: FileConfig,
    mut context: SourceContext,
) {
    let lease_file = config.path(&file_config);

    let registration = context.abort_registration();
    tokio::spawn(Abortable::new(
        async move {
            let records = if lease_file.exists() {
                match parse_file(&name, &lease_file) {
                    Ok(records) => records,
                    Err(e) => {
                        log::error!("({}) {}", name, e);
                        RecordSet::new()
                    }
                }
            } else {
                log::warn!("({}) file {} is missing.", name, lease_file.display());
                RecordSet::new()
            };

            context.send(records);

            let mut stream = match watch(&lease_file) {
                Ok(stream) => stream,
                Err(e) => {
                    log::error!("({}) {}", name, e);
                    return;
                }
            };

            while let Some(ev) = stream.next().await {
                let records = match ev {
                    FileEvent::Delete => {
                        log::warn!("({}) dhcp file {} is missing.", name, lease_file.display());
                        RecordSet::new()
                    }
                    _ => match parse_file(&name, &lease_file) {
                        Ok(records) => records,
                        Err(e) => {
                            log::error!("({}) {}", name, e);
                            RecordSet::new()
                        }
                    },
                };

                context.send(records);
            }
        },
        registration,
    ));
}
