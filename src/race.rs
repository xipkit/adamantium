//! Waiting for the first of several futures.

use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

/// A group of futures that [`race`] can run. It is implemented for tuples of
/// two to eight futures that have the same output.
pub trait Race {
    /// What the winning future returns.
    type Output;

    /// Runs the futures until the first one finishes.
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
/// part of a reply loses that work.
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
