#[cfg(feature = "tokio")]
use std::fmt;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

#[cfg(feature = "tokio")]
use tokio::time::Instant;

#[cfg(feature = "tokio")]
use crate::tasks::{Cancellation, Shutdown, shutdown};

pub trait Race {
    type Output;

    fn race(self) -> impl Future<Output = Self::Output>;
}

/// Waits for the first of several futures to finish and returns its output.
///
/// The futures run inside this one, in turn, on the task that awaits it. They
/// can borrow from the caller and need not be `Send` or `'static`. When one
/// finishes, the others are dropped before `race` returns, so none of them
/// runs afterwards.
///
/// Each time `race` is polled it polls the futures in the order written and
/// stops at the first that is ready. If two are ready together, the earlier
/// one wins and the later one is dropped without being polled again.
///
/// A future that loses is dropped at the await where it was waiting. Race
/// only futures that lose nothing by that: a receive, a sleep, or
/// `Cancellation::cancelled`. A future that has taken a message or written
/// part of a reply loses that work. To give a single drop-safe future a
/// `Cancellation` or a deadline, use [`until`](crate::until).
///
/// The futures must have the same output. To wait for different kinds of
/// event, give each future an `async` block that returns one variant of an
/// enum:
///
/// ```
/// use adamantium::{Channel, race};
///
/// enum Event {
///     Job(Option<u32>),
///     Stop(Option<()>),
/// }
///
/// # async fn example() {
/// let jobs = Channel::<u32>::bounded(4);
/// let stop = Channel::<()>::bounded(1);
/// jobs.send(7).await.ok();
///
/// let event = race((
///     async { Event::Stop(stop.receive().await) },
///     async { Event::Job(jobs.receive().await) },
/// ))
/// .await;
/// assert!(matches!(event, Event::Job(Some(7))));
/// # }
/// ```
///
/// To race work that should run in parallel, or to keep the first success
/// and ignore failures, use `first_ok`.
#[allow(
    clippy::future_not_send,
    reason = "the future is Send when the raced futures are"
)]
pub fn race<R: Race>(futures: R) -> impl Future<Output = R::Output> {
    futures.race()
}

macro_rules! race_for_tuple {
    ($($future:ident $name:ident),+) => {
        impl<T, $($future: Future<Output = T>),+> Race for ($($future,)+) {
            type Output = T;

            #[allow(
                clippy::future_not_send,
                reason = "the future is Send when the raced futures are"
            )]
            async fn race(self) -> T {
                let ($($name,)+) = self;
                $(let mut $name = pin!($name);)+
                poll_fn(|context| {
                    $(
                        if let Poll::Ready(output) = $name.as_mut().poll(context) {
                            return Poll::Ready(output);
                        }
                    )+
                    Poll::Pending
                })
                .await
            }
        }
    };
}

race_for_tuple!(A a, B b);
race_for_tuple!(A a, B b, C c);
race_for_tuple!(A a, B b, C c, D d);
race_for_tuple!(A a, B b, C c, D d, E e);
race_for_tuple!(A a, B b, C c, D d, E e, F f);
race_for_tuple!(A a, B b, C c, D d, E e, F f, G g);
race_for_tuple!(A a, B b, C c, D d, E e, F f, G g, H h);

#[cfg(feature = "tokio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntilError {
    Cancelled,
    DeadlineExceeded,
}

#[cfg(feature = "tokio")]
impl fmt::Display for UntilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("wait cancelled"),
            Self::DeadlineExceeded => f.write_str("wait deadline exceeded"),
        }
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for UntilError {}

/// Waits for `future` until `cancellation` is requested or the absolute
/// `deadline` passes. Pass `None` for no deadline. A deadline needs Tokio's
/// time driver.
///
/// A request already present when the call is made, or at any later poll, wins
/// over `future`, and cancellation wins over an equally ready deadline, as in
/// [`Tasks::run_until`](crate::Tasks::run_until).
///
/// If `until` stops before `future` finishes, `future` is dropped at the
/// `await` where it was waiting. Pass only futures that lose nothing when
/// dropped.
///
/// These library operations are safe to drop:
/// - [`Channel::receive`](crate::Channel::receive) and [`Receiver::receive`](crate::Receiver::receive)
/// - [`ReplyReceiver::receive`](crate::ReplyReceiver::receive) and [`Reply::closed`](crate::Reply::closed)
/// - [`Watcher::receive`](crate::Watcher::receive) and [`Watcher::wait_for`](crate::Watcher::wait_for)
/// - [`time::sleep`](crate::time::sleep) and [`time::sleep_until`](crate::time::sleep_until)
/// - [`Cancellation::cancelled`](crate::Cancellation::cancelled) (and `Cancellation::cancel_on_signal` with the `signal` feature)
///
/// Sending (`Channel::send`, `Sender::send`, `Topic::send`, `AsyncActor::send`)
/// is not safe to drop because dropping the future drops the unsent value;
/// use `send_until` instead. Operations with their own `_until` methods
/// (`receive_until`, `send_until`, `Tasks::run_until`, `Limit::run_until`,
/// `KeyedLimit::run_until`, `RateLimit::run_until`, `SingleFlight::run_until`)
/// already accept a `Cancellation` and deadline directly.
///
/// ```
/// use adamantium::{Cancellation, Reply, UntilError, time, until};
/// use std::time::Duration;
/// # #[tokio::main] async fn main() {
/// let (reply, _answer) = Reply::<u32>::pair();
/// let cancellation = Cancellation::new();
/// let soon = Some(time::now() + Duration::from_millis(10));
/// assert_eq!(
///     until(&cancellation, soon, reply.closed()).await,
///     Err(UntilError::DeadlineExceeded),
/// );
/// # }
/// ```
#[cfg(feature = "tokio")]
#[allow(
    clippy::future_not_send,
    reason = "the future is Send when the wrapped future is"
)]
pub async fn until<Fut: Future>(
    cancellation: &Cancellation,
    deadline: Option<Instant>,
    future: Fut,
) -> Result<Fut::Output, UntilError> {
    race((
        async {
            Err(match shutdown(cancellation, deadline).await {
                Shutdown::Cancelled => UntilError::Cancelled,
                Shutdown::DeadlineExceeded => UntilError::DeadlineExceeded,
            })
        },
        async { Ok(future.await) },
    ))
    .await
}
