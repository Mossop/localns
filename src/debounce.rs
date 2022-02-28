use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{Future, Stream};
use pin_project_lite::pin_project;
use tokio::time::{sleep, Sleep};

struct Pending<T> {
    item: T,
    delay: Pin<Box<Sleep>>,
}

pin_project! {
    pub struct Debounced<St>
    where
        St: Stream
    {
        #[pin]
        inner: St,
        delay: Duration,
        last: Option<St::Item>,
        pending: Option<Pending<St::Item>>,
    }
}

impl<St> Debounced<St>
where
    St: Stream,
{
    pub fn new(stream: St, delay: Duration) -> Self {
        Debounced {
            inner: stream,
            delay,
            last: None,
            pending: None,
        }
    }
}

impl<St> Stream for Debounced<St>
where
    St: Stream,
    St::Item: PartialEq + Clone,
{
    type Item = St::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this.inner.poll_next(cx) {
            Poll::Pending => (),
            Poll::Ready(Some(value)) => {
                if let Some(ref last) = this.last {
                    if last == &value {
                        // Force the task to re-poll
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }

                *this.pending = Some(Pending {
                    item: value.clone(),
                    delay: Box::pin(sleep(*this.delay)),
                });
            }
            Poll::Ready(None) => {
                return Poll::Ready(None);
            }
        }

        let pending = this.pending.take();

        match pending {
            Some(mut inner) => match inner.delay.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    this.last.replace(inner.item.clone());
                    Poll::Ready(Some(inner.item))
                }
                Poll::Pending => {
                    *this.pending = Some(inner);
                    Poll::Pending
                }
            },
            None => Poll::Pending,
        }
    }
}
