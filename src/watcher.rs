use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use notify::{Config, Event, EventHandler, PollWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc::{channel, Sender};

use crate::Error;

#[derive(Clone, Copy)]
pub(crate) enum FileEvent {
    Create,
    Delete,
    Change,
}

pub(crate) trait WatchListener
where
    Self: Send + 'static,
{
    fn event(&mut self, event: FileEvent) -> impl Future<Output = ()> + Send;
}

struct FileWatchState {
    sender: Sender<FileEvent>,
    path: PathBuf,
    exists: bool,
}

impl FileWatchState {
    fn new<L: WatchListener>(path: &Path, mut listener: L) -> Self {
        let (sender, mut receiver) = channel(10);

        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                listener.event(event).await;
            }
        });

        Self {
            path: path.to_owned(),
            exists: fs::exists(path).unwrap_or_default(),
            sender,
        }
    }
}

impl EventHandler for FileWatchState {
    fn handle_event(&mut self, event: Result<Event, notify::Error>) {
        if let Ok(event) = event {
            if !event.need_rescan() && !event.paths.contains(&self.path) {
                return;
            }
        }

        let now_exists = fs::exists(&self.path).unwrap_or_default();

        let event = match (now_exists, self.exists) {
            (true, false) => FileEvent::Create,
            (false, true) => FileEvent::Delete,
            (true, _) => FileEvent::Change,
            _ => return,
        };

        if let Err(e) = self.sender.blocking_send(event) {
            tracing::error!(error = %e, "Error sending file event")
        }
    }
}

pub(crate) struct Watcher {
    path: PathBuf,
    _watcher: PollWatcher,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        tracing::trace!(path = %self.path.display(), "Dropping file watcher");
    }
}

pub(crate) fn watch<L: WatchListener>(path: &Path, listener: L) -> Result<Watcher, Error> {
    tracing::trace!(path = %path.display(), "Starting file watcher");

    let watch_state = FileWatchState::new(path, listener);
    let mut watcher = PollWatcher::new(
        watch_state,
        Config::default().with_poll_interval(Duration::from_millis(500)),
    )?;

    let watch_path = match path.parent() {
        Some(parent) => parent,
        None => path,
    };

    watcher.watch(watch_path, RecursiveMode::Recursive)?;

    Ok(Watcher {
        path: path.to_owned(),
        _watcher: watcher,
    })
}
