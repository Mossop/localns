use std::{
    fs,
    future::Future,
    mem,
    path::{Path, PathBuf},
    time::Duration,
};

use notify::{Config, Event, EventHandler, PollWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::Error;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
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
    sender: UnboundedSender<FileEvent>,
    path: PathBuf,
    exists: bool,
}

impl FileWatchState {
    fn new<L: WatchListener>(path: &Path, mut listener: L) -> Self {
        let (sender, mut receiver) = unbounded_channel();

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
        if let Ok(ref event) = event {
            if !event.need_rescan() && !event.paths.contains(&self.path) {
                return;
            }
        }

        let now_exists = fs::exists(&self.path).unwrap_or_default();
        let mut did_exist = now_exists;
        mem::swap(&mut self.exists, &mut did_exist);

        let event = match (now_exists, did_exist) {
            (true, false) => FileEvent::Create,
            (false, true) => FileEvent::Delete,
            (true, _) => FileEvent::Change,
            _ => return,
        };

        if let Err(e) = self.sender.send(event) {
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

    let config = if cfg!(test) {
        Config::default()
            .with_poll_interval(Duration::from_millis(50))
            .with_compare_contents(true)
    } else {
        Config::default().with_poll_interval(Duration::from_millis(500))
    };

    let mut watcher = PollWatcher::new(watch_state, config)?;

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

#[cfg(test)]
mod tests {
    use std::{
        fs::{remove_file, File},
        io::Write,
    };

    use tempfile::TempDir;
    use tokio::sync::mpsc::{error::TryRecvError, unbounded_channel, UnboundedSender};

    use crate::watcher::{watch, FileEvent, WatchListener};

    struct ListenerStream {
        sender: UnboundedSender<FileEvent>,
    }

    impl WatchListener for ListenerStream {
        async fn event(&mut self, event: FileEvent) {
            self.sender.send(event).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher() {
        let (sender, mut receiver) = unbounded_channel();
        let listener = ListenerStream { sender };

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("test.txt");

        {
            let _watcher = watch(&target, listener).unwrap();

            assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

            File::create(&target).unwrap();

            let event = receiver.recv().await;
            assert_eq!(event, Some(FileEvent::Create));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Hello").unwrap();
            }

            let event = receiver.recv().await;
            assert_eq!(event, Some(FileEvent::Change));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Other").unwrap();
            }

            let event = receiver.recv().await;
            assert_eq!(event, Some(FileEvent::Change));

            remove_file(&target).unwrap();

            let event = receiver.recv().await;
            assert_eq!(event, Some(FileEvent::Delete));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Other").unwrap();
            }

            let event = receiver.recv().await;
            assert_eq!(event, Some(FileEvent::Create));
        }

        let event = receiver.recv().await;
        assert_eq!(event, None);
    }
}
