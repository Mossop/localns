use std::{
    fs,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::Stream;
use notify::{Config, Event, EventHandler, PollWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub enum FileEvent {
    Create,
    Delete,
    Change,
}

pub struct FileEventStream {
    _watcher: PollWatcher,
    receiver: Receiver<FileEvent>,
}

impl Stream for FileEventStream {
    type Item = FileEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

struct FileWatchState {
    path: PathBuf,
    exists: bool,
    sender: Sender<FileEvent>,
}

impl FileWatchState {
    fn new(path: &Path, sender: Sender<FileEvent>) -> Self {
        Self {
            path: path.to_owned(),
            exists: fs::exists(path).unwrap_or_default(),
            sender,
        }
    }
}

impl EventHandler for FileWatchState {
    fn handle_event(&mut self, _: Result<Event, notify::Error>) {
        let now_exists = fs::exists(&self.path).unwrap_or_default();

        let event = match (now_exists, self.exists) {
            (true, false) => FileEvent::Create,
            (false, true) => FileEvent::Delete,
            (true, _) => FileEvent::Change,
            _ => return,
        };

        let _ = self.sender.blocking_send(event);
    }
}

pub fn watch(path: &Path) -> Result<FileEventStream, String> {
    let (sender, receiver) = channel(10);

    let watch_state = FileWatchState::new(path, sender);
    let mut watcher = PollWatcher::new(
        watch_state,
        Config::default().with_poll_interval(Duration::from_millis(500)),
    )
    .map_err(|e| format!("Unable to watch file {}: {}", path.display(), e))?;

    let watch_path = match path.parent() {
        Some(parent) => parent,
        None => path,
    };

    watcher
        .watch(watch_path, RecursiveMode::Recursive)
        .map_err(|e| format!("Unable to watch file {}: {}", path.display(), e))?;

    Ok(FileEventStream {
        _watcher: watcher,
        receiver,
    })
}
