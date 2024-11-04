use std::{cmp::min, future::Future, time::Duration};

use tokio::time::sleep;

use crate::{sources::SourceId, RecordServer};

pub(crate) enum LoopResult {
    Sleep,
    Backoff,
    Quit,
}

#[derive(Clone)]
pub(crate) struct RunLoop {
    default: u64,
    scaling: f64,
    max: u64,
    current: u64,
}

impl RunLoop {
    pub(crate) fn new(interval: u64) -> Self {
        RunLoop {
            default: interval,
            scaling: 1.2,
            max: interval * 10,
            current: interval,
        }
    }

    fn reset(&mut self) -> Duration {
        self.current = self.default;

        Duration::from_millis(self.current)
    }

    fn backoff(&mut self) -> Duration {
        self.current = min(
            ((self.current as f64) * self.scaling).round() as u64,
            self.max,
        );

        Duration::from_millis(self.current)
    }

    pub(crate) async fn run<S, F, C>(mut self, server: S, source_id: SourceId, mut cb: C)
    where
        S: RecordServer,
        F: Future<Output = LoopResult>,
        C: FnMut(S, SourceId) -> F,
    {
        loop {
            let result = cb(server.clone(), source_id.clone()).await;

            let duration = match result {
                LoopResult::Sleep => self.reset(),
                LoopResult::Backoff => {
                    server.clear_source_records(&source_id).await;
                    self.backoff()
                }
                LoopResult::Quit => {
                    server.clear_source_records(&source_id).await;
                    return;
                }
            };

            sleep(duration).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let assert_duration =
            |duration: Duration, millis: u128| assert_eq!(duration.as_millis(), millis);

        let mut backoff = RunLoop::new(200);
        backoff.scaling = 2.5;

        assert_duration(backoff.reset(), 200);
        assert_duration(backoff.reset(), 200);
        assert_duration(backoff.reset(), 200);
        assert_duration(backoff.backoff(), 500);
        assert_duration(backoff.backoff(), 1250);
        assert_duration(backoff.reset(), 200);
        assert_duration(backoff.backoff(), 500);
        assert_duration(backoff.backoff(), 1250);
        assert_duration(backoff.backoff(), 2000);
        assert_duration(backoff.backoff(), 2000);
        assert_duration(backoff.backoff(), 2000);
        assert_duration(backoff.reset(), 200);
        assert_duration(backoff.backoff(), 500);
    }
}
