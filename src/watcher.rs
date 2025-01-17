use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{fs::File, io::AsyncReadExt, task::JoinHandle, time::sleep};

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

pub(crate) struct Watcher {
    handle: JoinHandle<()>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Watcher {
    async fn fetch_state(path: &Path) -> Option<[u8; 32]> {
        let mut file = File::open(path).await.ok()?;
        let mut buffer = [0_u8; 65536];

        let mut hasher = Sha256::new();

        loop {
            let len = file.read(&mut buffer).await.ok()?;

            if len == 0 {
                break;
            }

            hasher.update(&buffer[0..len]);
        }

        let mut output = [0_u8; 32];
        output.copy_from_slice(hasher.finalize().as_slice());

        Some(output)
    }

    async fn watch_loop<L: WatchListener>(
        path: PathBuf,
        interval: Duration,
        mut state: Option<[u8; 32]>,
        mut listener: L,
    ) {
        loop {
            sleep(interval).await;

            let new_state = Watcher::fetch_state(&path).await;

            if new_state != state {
                let event = match (state, new_state) {
                    (None, _) => FileEvent::Create,
                    (_, None) => FileEvent::Delete,
                    _ => FileEvent::Change,
                };

                listener.event(event).await;

                state = new_state;
            }
        }
    }
}

pub(crate) async fn watch<L: WatchListener>(path: &Path, listener: L) -> Result<Watcher, Error> {
    tracing::trace!(path = %path.display(), "Starting file watcher");

    let initial_state = Watcher::fetch_state(path).await;

    let interval = if cfg!(test) {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(500)
    };

    let handle = tokio::spawn(Watcher::watch_loop(
        path.to_owned(),
        interval,
        initial_state,
        listener,
    ));

    Ok(Watcher { handle })
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{remove_file, File},
        io::Write,
    };

    use tempfile::TempDir;
    use tokio::sync::mpsc::{error::TryRecvError, unbounded_channel, UnboundedSender};

    use crate::{
        test::timeout,
        watcher::{watch, FileEvent, WatchListener},
    };

    impl WatchListener for UnboundedSender<FileEvent> {
        async fn event(&mut self, event: FileEvent) {
            self.send(event).unwrap();
        }
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    async fn watcher() {
        let (sender, mut receiver) = unbounded_channel();

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("test.txt");

        {
            let _watcher = watch(&target, sender).await.unwrap();

            assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

            File::create(&target).unwrap();

            let event = timeout(receiver.recv()).await;
            assert_eq!(event, Some(FileEvent::Create));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Hello").unwrap();
            }

            let event = timeout(receiver.recv()).await;
            assert_eq!(event, Some(FileEvent::Change));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Other").unwrap();
            }

            let event = timeout(receiver.recv()).await;
            assert_eq!(event, Some(FileEvent::Change));

            remove_file(&target).unwrap();

            let event = timeout(receiver.recv()).await;
            assert_eq!(event, Some(FileEvent::Delete));

            {
                let mut file = File::create(&target).unwrap();
                write!(file, "Other").unwrap();
            }

            let event = timeout(receiver.recv()).await;
            assert_eq!(event, Some(FileEvent::Create));
        }

        let event = timeout(receiver.recv()).await;
        assert_eq!(event, None);
    }
}
