use std::{cmp::min, future::Future, time::Duration};

use chrono::Utc;
use tokio::time::sleep;

use crate::{sources::SourceId, RecordServer};

pub(crate) enum LoopResult {
    Sleep,
    Backoff,
    Quit,
}

pub(crate) struct Backoff {
    default: u64,
    scaling: f64,
    max: u64,
    current: u64,
}

impl Backoff {
    pub(crate) fn new(interval: u64) -> Self {
        Backoff {
            default: interval,
            scaling: 1.2,
            max: interval * 10,
            current: interval,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.current = self.default;
    }

    pub(crate) fn backoff(&mut self) {
        self.current = min(
            ((self.current as f64) * self.scaling).round() as u64,
            self.max,
        );
    }

    pub(crate) fn duration(&self) -> Duration {
        Duration::from_millis(self.current)
    }
}

pub(crate) struct RunLoop {
    backoff: Backoff,
}

impl RunLoop {
    pub(crate) fn new(interval: u64) -> Self {
        RunLoop {
            backoff: Backoff::new(interval),
        }
    }

    pub(crate) async fn run<S, F, C>(mut self, server: S, source_id: SourceId, mut cb: C)
    where
        S: RecordServer,
        F: Future<Output = LoopResult>,
        C: FnMut(S, SourceId) -> F,
    {
        loop {
            let result = cb(server.clone(), source_id.clone()).await;

            match result {
                LoopResult::Sleep => self.backoff.reset(),
                LoopResult::Backoff => {
                    server.clear_source_records(&source_id, Utc::now()).await;
                    self.backoff.backoff()
                }
                LoopResult::Quit => {
                    server.clear_source_records(&source_id, Utc::now()).await;
                    return;
                }
            };

            sleep(self.backoff.duration()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let mut backoff = Backoff::new(200);
        backoff.scaling = 2.5;

        let assert_duration =
            |backoff: &Backoff, millis: u128| assert_eq!(backoff.duration().as_millis(), millis);

        backoff.reset();
        assert_duration(&backoff, 200);
        backoff.reset();
        assert_duration(&backoff, 200);
        backoff.reset();
        assert_duration(&backoff, 200);
        backoff.backoff();
        assert_duration(&backoff, 500);
        backoff.backoff();
        assert_duration(&backoff, 1250);
        backoff.reset();
        assert_duration(&backoff, 200);
        backoff.backoff();
        assert_duration(&backoff, 500);
        backoff.backoff();
        assert_duration(&backoff, 1250);
        backoff.backoff();
        assert_duration(&backoff, 2000);
        assert_duration(&backoff, 2000);
        assert_duration(&backoff, 2000);
        backoff.reset();
        assert_duration(&backoff, 200);
        backoff.backoff();
        assert_duration(&backoff, 500);
    }
}
