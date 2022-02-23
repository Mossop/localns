use std::time::{Duration, Instant};

use futures::future::{AbortHandle, Abortable};
use tokio::{
    select,
    sync::mpsc::{channel, Receiver, Sender},
    time::sleep,
};

pub struct Debounced {
    abort: AbortHandle,
    sender: Sender<()>,
}

fn trigger_delay(
    first_event: &Instant,
    delay: &Duration,
    max_delay: &Option<Duration>,
) -> Duration {
    if let Some(max) = max_delay {
        let current_duration = Instant::now().duration_since(*first_event);
        if current_duration > *max {
            return Duration::from_millis(0);
        }

        let remaining = *max - current_duration;
        if remaining < *delay {
            return remaining;
        }
    }

    return delay.clone();
}

async fn debounce_loop<F>(
    cb: F,
    mut receiver: Receiver<()>,
    delay: Duration,
    max_delay: Option<Duration>,
) where
    F: Fn() + Send,
{
    log::trace!("Entering debounce loop.");

    loop {
        log::trace!("Waiting for first trigger.");
        receiver.recv().await;
        log::trace!("Received trigger.");

        let first_event = Instant::now();

        loop {
            let delay = trigger_delay(&first_event, &delay, &max_delay);
            log::trace!("Delaying for {}ms.", delay.as_millis());

            select! {
                _ = sleep(delay) => {
                    log::trace!("Debounce complete.");
                    cb();
                    break;
                }
                _ = receiver.recv() => {
                    log::trace!("Received re-trigger.");
                }
            }
        }
    }
}

impl Debounced {
    pub fn new<F>(cb: F, delay: u64, max_delay: Option<u64>) -> Self
    where
        F: Fn() + Send + 'static,
    {
        let (abort, registration) = AbortHandle::new_pair();
        let (sender, receiver) = channel::<()>(2);

        tokio::spawn(async move {
            Abortable::new(
                debounce_loop(
                    cb,
                    receiver,
                    Duration::from_millis(delay),
                    max_delay.map(Duration::from_millis),
                ),
                registration,
            )
            .await
        });

        Debounced { abort, sender }
    }

    pub fn trigger(&self) {
        let sender = self.sender.clone();
        tokio::spawn(async move {
            if let Err(e) = sender.send(()).await {
                log::error!("Unexpected failure to trigger: {}", e)
            }
        });
    }
}

impl Drop for Debounced {
    fn drop(&mut self) {
        self.abort.abort()
    }
}
