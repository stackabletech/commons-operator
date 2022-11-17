use std::fmt::Debug;

use futures::channel;
use snafu::Snafu;
use tokio::sync::RwLock;

/// The sending counterpart to a [`DelayedInit`]
pub struct Initializer<T>(channel::oneshot::Sender<T>);
impl<T> Initializer<T> {
    /// Sends `value` to the linked [`DelayedInit`].
    pub fn init(self, value: T) {
        // oneshot::Sender::send fails if no recipients remain, this is not really a relevant
        // case to signal for our use case
        let _ = self.0.send(value);
    }
}

/// A value that must be initialized by an external writer
///
/// Can be considered equivalent to a [`channel::oneshot`] channel, except for that
/// the value produced is retained for subsequent calls to [`Self::get`].
#[derive(Debug)]
pub struct DelayedInit<T>(RwLock<ReceiverState<T>>);
#[derive(Debug)]
enum ReceiverState<T> {
    Waiting(channel::oneshot::Receiver<T>),
    Ready(Result<T, InitDropped>),
}
impl<T> DelayedInit<T> {
    /// Returns an empty `DelayedInit` that has no value, along with a linked [`Initializer`]
    pub fn new() -> (Initializer<T>, Self) {
        let (tx, rx) = channel::oneshot::channel();
        (
            Initializer(tx),
            DelayedInit(RwLock::new(ReceiverState::Waiting(rx))),
        )
    }
}
impl<T: Clone + Debug> DelayedInit<T> {
    /// Wait for the value to be available and then return it
    ///
    /// Calling `get` again if a value has already been returned is guaranteed to return (a clone of)
    /// the same value.
    #[tracing::instrument(name = "DelayedInit::get", level = "trace")]
    pub async fn get(&self) -> Result<T, InitDropped> {
        let read_lock = self.0.read().await;
        if let ReceiverState::Ready(v) = &*read_lock {
            tracing::trace!("using fast path, value is already ready");
            v.clone()
        } else {
            tracing::trace!("using slow path, need to wait for the channel");
            // IMPORTANT: Make sure that the optimistic read lock has been released already
            drop(read_lock);
            let mut state = self.0.write().await;
            tracing::trace!("got write lock");
            match &mut *state {
                ReceiverState::Waiting(rx) => {
                    tracing::trace!("channel still active, awaiting");
                    let value = rx.await.map_err(|_| InitDropped);
                    tracing::trace!("got value on slow path, memoizing");
                    *state = ReceiverState::Ready(value.clone());
                    value
                }
                ReceiverState::Ready(v) => {
                    tracing::trace!("slow path but value was already initialized, another writer already initialized");
                    v.clone()
                }
            }
        }
    }
}

#[derive(Debug, Snafu, Clone, Copy, PartialEq, Eq)]
#[snafu(display("initializer was dropped before value was initialized"))]
pub struct InitDropped;

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use futures::{pin_mut, poll};
    use tracing::Level;
    use tracing_subscriber::util::SubscriberInitExt;

    use super::DelayedInit;

    fn setup_tracing() -> tracing::dispatcher::DefaultGuard {
        tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .with_test_writer()
            .finish()
            .set_default()
    }

    #[tokio::test]
    async fn must_allow_single_reader() {
        let _tracing = setup_tracing();
        let (tx, rx) = DelayedInit::<u8>::new();
        let get1 = rx.get();
        pin_mut!(get1);
        assert_eq!(poll!(get1.as_mut()), Poll::Pending);
        tx.init(1);
        assert_eq!(poll!(get1), Poll::Ready(Ok(1)));
    }

    #[tokio::test]
    async fn must_allow_concurrent_readers_while_waiting() {
        let _tracing = setup_tracing();
        let (tx, rx) = DelayedInit::<u8>::new();
        let get1 = rx.get();
        let get2 = rx.get();
        let get3 = rx.get();
        pin_mut!(get1, get2, get3);
        assert_eq!(poll!(get1.as_mut()), Poll::Pending);
        assert_eq!(poll!(get2.as_mut()), Poll::Pending);
        assert_eq!(poll!(get3.as_mut()), Poll::Pending);
        tx.init(1);
        assert_eq!(poll!(get1), Poll::Ready(Ok(1)));
        assert_eq!(poll!(get2), Poll::Ready(Ok(1)));
        assert_eq!(poll!(get3), Poll::Ready(Ok(1)));
    }

    #[tokio::test]
    async fn must_allow_reading_after_init() {
        let _tracing = setup_tracing();
        let (tx, rx) = DelayedInit::<u8>::new();
        let get1 = rx.get();
        pin_mut!(get1);
        assert_eq!(poll!(get1.as_mut()), Poll::Pending);
        tx.init(1);
        assert_eq!(poll!(get1), Poll::Ready(Ok(1)));
        assert_eq!(rx.get().await, Ok(1));
        assert_eq!(rx.get().await, Ok(1));
    }

    #[tokio::test]
    async fn must_allow_concurrent_readers_in_any_order() {
        let _tracing = setup_tracing();
        let (tx, rx) = DelayedInit::<u8>::new();
        let get1 = rx.get();
        let get2 = rx.get();
        let get3 = rx.get();
        pin_mut!(get1, get2, get3);
        assert_eq!(poll!(get1.as_mut()), Poll::Pending);
        assert_eq!(poll!(get2.as_mut()), Poll::Pending);
        assert_eq!(poll!(get3.as_mut()), Poll::Pending);
        tx.init(1);
        assert_eq!(poll!(get3), Poll::Ready(Ok(1)));
        assert_eq!(poll!(get2), Poll::Ready(Ok(1)));
        assert_eq!(poll!(get1), Poll::Ready(Ok(1)));
    }
}
