use std::{sync::Arc, task::Poll};

use futures::{
    future::{self, FusedFuture},
    stream, Future, FutureExt, SinkExt, Stream, StreamExt,
};
use pin_project::pin_project;
use tokio::time::{sleep_until, Instant, Sleep};

#[cfg(doc)]
use stackable_operator::kube;

/// Runs `applier` whenever `watcher` emits a new object
///
/// `applier` returns an `Instant` for the next reconciliation, at which point
/// `applier` will be executed again (with the same object) _if_ `watcher` has
/// not emitted a new object yet.
///
/// The intention is that `watcher` will be the future returned by
/// [`kube::runtime::watcher::watch_object`].
pub fn single_object_applier<
    K,
    F: Fn(Option<Arc<K>>) -> Fut,
    Fut: Future<Output = Option<Instant>>,
    W: Stream<Item = Option<K>>,
>(
    watcher: W,
    reconciler: F,
) -> impl Stream<Item = ()> {
    let (reschedule_tx, reschedule_rx) = futures::channel::mpsc::channel(1);
    LastObjectEmitter::new(
        watcher.map(|o| o.map(Arc::new)),
        Scheduler::new(reschedule_rx),
    )
    .then(reconciler)
    .then(move |reschedule_after| {
        let mut reschedule_tx = reschedule_tx.clone();
        async move { reschedule_tx.send(reschedule_after).await.unwrap() }
    })
}

/// Reemits the latest object emitted by `source` whenever `reemit_trigger` emits a value
///
/// Will skip intermedia values if `source` emits multiple values at once.
/// Will only emit one value, even if `reemit_trigger` has emitted multiple values at once.
/// Will terminate once `source` terminates.
#[pin_project]
struct LastObjectEmitter<S1: Stream, S2> {
    #[pin]
    source: stream::Fuse<S1>,
    #[pin]
    reemit_trigger: stream::Fuse<S2>,
    current_value: Option<S1::Item>,
}

impl<S1: Stream, S2: Stream> LastObjectEmitter<S1, S2> {
    fn new(source: S1, reemit_trigger: S2) -> Self {
        Self {
            source: source.fuse(),
            reemit_trigger: reemit_trigger.fuse(),
            current_value: None,
        }
    }
}

impl<S1: Stream, S2: Stream<Item = ()>> Stream for LastObjectEmitter<S1, S2>
where
    S1::Item: Clone,
{
    type Item = S1::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut reemit = false;
        let mut this = self.project();
        while let Poll::Ready(Some(v)) = this.source.as_mut().poll_next(cx) {
            // Skip in-between states if newer values are available
            *this.current_value = Some(v);
            reemit = true;
        }
        while let Poll::Ready(Some(_)) = this.reemit_trigger.as_mut().poll_next(cx) {
            reemit = true;
        }
        match this.current_value {
            Some(v) if reemit => Poll::Ready(Some(v.clone())),
            _ if this.source.is_done() => Poll::Ready(None),
            _ => Poll::Pending,
        }
    }
}

/// Emits a value once the [`Instant`] emitted by `requests` has been reached.
///
/// If another [`Instant`] is emitted by `requests` then the current schedule will be cancelled,
/// and the new time preferred instead.
///
/// Terminates once `requests` has terminated, and the currently active request has been served.
#[pin_project(project = SchedulerProj)]
struct Scheduler<S> {
    #[pin]
    requests: stream::Fuse<S>,
    #[pin]
    current_sleep: future::Fuse<Sleep>,
}

impl<S: Stream> Scheduler<S> {
    fn new(requests: S) -> Self {
        Self {
            requests: requests.fuse(),
            current_sleep: future::Fuse::terminated(),
        }
    }
}

impl<S: Stream<Item = Option<Instant>>> Stream for Scheduler<S> {
    type Item = ();

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let mut this = self.project();
        while let Poll::Ready(Some(s)) = this.requests.as_mut().poll_next(cx) {
            this.current_sleep
                .set(s.map_or(future::Fuse::terminated(), |s| sleep_until(s).fuse()));
        }
        if let Poll::Ready(()) = this.current_sleep.as_mut().poll(cx) {
            Poll::Ready(Some(()))
        } else if this.current_sleep.is_terminated() && this.requests.is_done() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}
