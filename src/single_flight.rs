use std::{
    collections::HashMap,
    fmt,
    future::{Future, poll_fn},
    hash::Hash,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use crate::runtime::{
    Guard, Lock, World,
    wait::{WaitQueue, Waiter},
};
#[cfg(feature = "tokio")]
use crate::{Cancellation, ScopeError, race, tasks::shutdown};
#[cfg(feature = "tokio")]
use tokio::time::Instant;

/// Shares one operation and its result among concurrent calls for a key.
///
/// [`Self::run`] starts or joins an operation. Each caller gets a copy of its
/// result, including errors. Waiting callers can finish the operation if its
/// creator leaves. The last caller to leave drops any unfinished operation.
/// Clones share in-flight work. Completed results are never cached.
///
/// ```
/// use adamantium::SingleFlight;
/// # async fn fetch(user: u32) -> Result<String, String> { Ok(format!("user {user}")) }
///
/// # async fn example() {
/// let profiles = SingleFlight::new();
/// // Concurrent calls for user 7 make one fetch and share its result.
/// let profile = profiles.run(7, || fetch(7)).await;
/// assert_eq!(profile.as_deref(), Ok("user 7"));
/// # }
/// ```
///
/// # Sharing or serializing
///
/// A caller can join an operation that started before it arrived. For writes,
/// have the operation report which changes it covered and retry if needed.
/// Cleanup from a dropped operation may overlap the next operation. Use a
/// [`KeyedLimit`](crate::KeyedLimit) when each caller needs a separate
/// operation or operations must run in sequence.
///
/// # Panics
///
/// An operation panic propagates to all joined callers. A direct call to
/// `run` for the operation's own key panics. An indirect cycle through a
/// spawned task can still deadlock.
pub struct SingleFlight<K, T> {
    shared: Arc<Shared<K, T>>,
}

type Operation<T> = Pin<Box<dyn Future<Output = T> + Send>>;

struct Shared<K, T> {
    // Remove the flight under the same lock that changes its stage so new
    // calls cannot join a completed flight.
    flights: Lock<HashMap<K, Arc<Flight<T>>>>,
    world: World,
}

struct Flight<T> {
    // Locked after `Shared::flights` when both are needed.
    state: Lock<State<T>>,
    signal: Arc<Signal>,
    // Every caller polls the operation with the same waker.
    waker: Waker,
    world: World,
}

struct State<T> {
    stage: Stage<T>,
    callers: usize,
}

enum Stage<T> {
    Idle(Operation<T>),
    // A caller has taken the operation out to poll it, or to create it.
    Polling,
    Finished(Arc<T>),
    Panicked,
    Ended,
}

// A retained waker keeps only Signal alive.
struct Signal {
    // Set when the operation is woken, and cleared by the caller that
    // takes it to poll it.
    woken: AtomicBool,
    changed: WaitQueue,
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.changed.notify_all_from_waker();
    }
}

impl<K, T> SingleFlight<K, T> {
    /// A `SingleFlight` with no operations in flight.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                flights: Lock::new(HashMap::new()),
                world: World::current(),
            }),
        }
    }

    /// The number of keys with an operation in flight.
    pub fn len(&self) -> usize {
        self.shared.lock("SingleFlight::len").len()
    }

    /// Whether no operation is in flight.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K, T> SingleFlight<K, T>
where
    K: Hash + Eq + Clone,
    T: Clone + Send + 'static,
{
    /// Runs `operation` for `key`, or joins the operation already in flight
    /// for it, and returns a copy of its output.
    ///
    /// `operation` is called only when no operation for `key` is in flight;
    /// a call that joins drops it without calling it. A call joins when it
    /// is first polled. Dropping the future leaves the operation to the
    /// other callers, and drops it if there are none.
    ///
    /// # Panics
    ///
    /// Panics if the operation for `key` makes this call itself, or if the
    /// operation panics.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the key and the operation are"
    )]
    pub async fn run<F, Fut>(&self, key: K, operation: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let mut caller = self.join(key);
        let mut held = None;
        if caller.polling {
            let mut creating = Unwinding::arm(&mut caller);
            // Creating the operation is part of it, so a call it makes for
            // its own key is refused here too.
            let polling = Polling::enter(&creating.caller.flight);
            held = Some(Box::pin(operation()) as Operation<T>);
            drop(polling);
            creating.disarm();
        } else {
            drop(operation);
        }
        let mut waiter = None;
        poll_fn(move |cx| caller.poll(cx, &mut held, &mut waiter)).await
    }

    fn join(&self, key: K) -> Caller<K, T> {
        let mut flights = self.shared.lock("SingleFlight::join");
        if let Some(flight) = flights.get(&key) {
            let flight = Arc::clone(flight);
            if polling_here(&flight) {
                drop(flights);
                refuse_self_join();
            }
            flight.lock("SingleFlight::join").callers += 1;
            drop(flights);
            return Caller {
                shared: Arc::clone(&self.shared),
                key,
                flight,
                polling: false,
                done: false,
            };
        }
        let flight = Arc::new(Flight::new(self.shared.world));
        flights.insert(key.clone(), Arc::clone(&flight));
        drop(flights);
        Caller {
            shared: Arc::clone(&self.shared),
            key,
            flight,
            polling: true,
            done: false,
        }
    }
}

#[cfg(feature = "tokio")]
impl<K, T, E> SingleFlight<K, Result<T, E>>
where
    K: Hash + Eq + Clone,
    T: Clone + Send + 'static,
    E: Clone + Send + 'static,
{
    /// Shares an operation until it completes, cancellation is requested, or
    /// the deadline passes. Build deadlines with [`crate::time`] or pass
    /// `None` for no deadline.
    ///
    /// Each poll checks cancellation and then the deadline before joining
    /// the flight or taking its result. A caller that stops leaves the
    /// operation running for other callers. The last caller drops it.
    /// Returns [`ScopeError::Failed`] for an operation error.
    ///
    /// # Panics
    ///
    /// Panics if the operation for `key` makes this call itself, or if the
    /// operation panics.
    #[allow(
        clippy::future_not_send,
        reason = "the future is Send when the key and the operation are"
    )]
    pub async fn run_until<F, Fut>(
        &self,
        key: K,
        cancellation: &Cancellation,
        deadline: Option<Instant>,
        operation: F,
    ) -> Result<T, ScopeError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        // The race drops the losing call, which leaves the flight before return.
        race((
            async { Err(shutdown(cancellation, deadline).await.scope_error()) },
            async { self.run(key, operation).await.map_err(ScopeError::Failed) },
        ))
        .await
    }
}

impl<K, T> Shared<K, T> {
    // Every operation locks, so this is where a handle carried into another
    // harness execution is rejected. While unwinding the check is skipped,
    // so that a dropped call can still leave its flight.
    fn lock(&self, operation: &'static str) -> Guard<'_, HashMap<K, Arc<Flight<T>>>> {
        self.world.check_unless_panicking();
        self.flights.lock(operation)
    }
}

impl<T> Flight<T> {
    fn new(world: World) -> Self {
        let signal = Arc::new(Signal {
            woken: AtomicBool::new(false),
            changed: WaitQueue::named("single_flight.wait"),
        });
        Self {
            state: Lock::new(State {
                stage: Stage::Polling,
                callers: 1,
            }),
            waker: Waker::from(Arc::clone(&signal)),
            signal,
            world,
        }
    }

    fn lock(&self, operation: &'static str) -> Guard<'_, State<T>> {
        self.world.check_unless_panicking();
        self.state.lock(operation)
    }
}

struct Caller<K: Hash + Eq, T> {
    shared: Arc<Shared<K, T>>,
    key: K,
    flight: Arc<Flight<T>>,
    polling: bool,
    done: bool,
}

impl<K: Hash + Eq, T: Clone> Caller<K, T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        held: &mut Option<Operation<T>>,
        waiter: &mut Option<Waiter>,
    ) -> Poll<T> {
        // Poll the operation once per call poll so self-wakes yield to the executor.
        let mut polled_once = false;
        loop {
            let mut operation = match held.take() {
                Some(operation) => operation,
                None => match self.take_or_collect(waiter, !polled_once) {
                    Taken::Operation(operation) => operation,
                    Taken::Output(output) => return Poll::Ready(output),
                    Taken::Yield => {
                        // If this call is dropped before it is polled again,
                        // its drop hands the pending wake to the others.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Taken::Nothing => {
                        let Some(listening) = waiter.as_mut() else {
                            continue;
                        };
                        match Pin::new(listening).poll(cx) {
                            Poll::Ready(()) => {
                                *waiter = None;
                                continue;
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                },
            };
            let polled = {
                let mut unwinding = Unwinding::arm(self);
                let _polling = Polling::enter(&unwinding.caller.flight);
                let polled = operation
                    .as_mut()
                    .poll(&mut Context::from_waker(&unwinding.caller.flight.waker));
                unwinding.disarm();
                polled
            };
            polled_once = true;
            match polled {
                Poll::Ready(output) => return Poll::Ready(self.finish(output)),
                Poll::Pending => {
                    self.flight.lock("Caller::poll").stage = Stage::Idle(operation);
                    self.polling = false;
                }
            }
        }
    }

    fn take_or_collect(&mut self, waiter: &mut Option<Waiter>, may_poll: bool) -> Taken<T> {
        // Register before checking for a wake or completion.
        if waiter.is_none() {
            *waiter = Some(self.flight.signal.changed.listen());
        }
        let mut state = self.flight.lock("Caller::take_or_collect");
        match &mut state.stage {
            Stage::Idle(_) if !may_poll => {
                if self.flight.signal.woken.load(Ordering::SeqCst) {
                    Taken::Yield
                } else {
                    Taken::Nothing
                }
            }
            Stage::Idle(_) if self.flight.signal.woken.swap(false, Ordering::SeqCst) => {
                match std::mem::replace(&mut state.stage, Stage::Polling) {
                    Stage::Idle(operation) => {
                        self.polling = true;
                        Taken::Operation(operation)
                    }
                    other => {
                        state.stage = other;
                        Taken::Nothing
                    }
                }
            }
            Stage::Finished(output) => {
                let output = Arc::clone(output);
                state.callers -= 1;
                // The last caller leaves the flight holding no output, so
                // its copy is the original unless another caller still
                // holds theirs.
                let left =
                    (state.callers == 0).then(|| std::mem::replace(&mut state.stage, Stage::Ended));
                drop(state);
                drop(left);
                self.done = true;
                Taken::Output(Arc::unwrap_or_clone(output))
            }
            Stage::Panicked => {
                state.callers -= 1;
                drop(state);
                self.done = true;
                operation_panicked()
            }
            Stage::Idle(_) | Stage::Polling | Stage::Ended => Taken::Nothing,
        }
    }

    fn finish(&mut self, output: T) -> T {
        let mut flights = self.shared.lock("Caller::finish");
        let mut state = self.flight.lock("Caller::finish");
        state.callers -= 1;
        let output = if state.callers == 0 {
            state.stage = Stage::Ended;
            Ok(output)
        } else {
            let output = Arc::new(output);
            state.stage = Stage::Finished(Arc::clone(&output));
            Err(output)
        };
        self.forget(&mut flights);
        drop(state);
        drop(flights);
        self.polling = false;
        self.done = true;
        self.flight.signal.changed.notify_all();
        output.unwrap_or_else(Arc::unwrap_or_clone)
    }

    fn abandon(&mut self) {
        let mut flights = self.shared.lock("Caller::abandon");
        let mut state = self.flight.lock("Caller::abandon");
        state.callers -= 1;
        state.stage = Stage::Panicked;
        self.forget(&mut flights);
        drop(state);
        drop(flights);
        self.polling = false;
        self.done = true;
        self.flight.signal.changed.notify_all();
    }
}

impl<K: Hash + Eq, T> Caller<K, T> {
    // Removes this call's flight from the operations in flight, unless a
    // newer flight has already replaced it.
    fn forget(&self, flights: &mut HashMap<K, Arc<Flight<T>>>) {
        if flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

enum Taken<T> {
    Operation(Operation<T>),
    Output(T),
    Yield,
    Nothing,
}

impl<K: Hash + Eq, T> Drop for Caller<K, T> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut flights = self.shared.lock("Caller::drop");
        let mut state = self.flight.lock("Caller::drop");
        state.callers -= 1;
        if state.callers > 0 {
            // Pass a pending wake to the remaining callers when this caller leaves.
            let woken = matches!(state.stage, Stage::Idle(_))
                && self.flight.signal.woken.load(Ordering::SeqCst);
            drop(state);
            drop(flights);
            if woken {
                self.flight.signal.changed.notify_all();
            }
            return;
        }
        let stage = match &state.stage {
            Stage::Idle(_) | Stage::Finished(_) => {
                std::mem::replace(&mut state.stage, Stage::Ended)
            }
            // A caller polling the operation is still in the flight, and
            // a panicked flight has already been removed.
            Stage::Polling | Stage::Panicked | Stage::Ended => return,
        };
        self.forget(&mut flights);
        drop(state);
        drop(flights);
        // Drop user data after releasing both locks. A new call may start meanwhile.
        drop(stage);
    }
}

// Ends the flight as panicked if the operation panics while this call is
// creating or polling it, so the other callers don't wait for it.
struct Unwinding<'a, K: Hash + Eq, T: Clone> {
    caller: &'a mut Caller<K, T>,
    armed: bool,
}

impl<'a, K: Hash + Eq, T: Clone> Unwinding<'a, K, T> {
    fn arm(caller: &'a mut Caller<K, T>) -> Self {
        Self {
            caller,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<K: Hash + Eq, T: Clone> Drop for Unwinding<'_, K, T> {
    fn drop(&mut self) {
        if self.armed {
            self.caller.abandon();
        }
    }
}

const PANICKED: &str =
    "adamantium: the operation this SingleFlight::run call was sharing panicked in another call";

#[allow(
    clippy::panic,
    reason = "the operation has no output to share, and its panic is reported where it happened"
)]
fn operation_panicked() -> ! {
    std::panic::panic_any(PANICKED)
}

const SELF_JOIN: &str = "adamantium: SingleFlight::run was called by the operation for the same key; \
it would wait for its own output, so call it from outside the operation";

#[allow(
    clippy::panic,
    reason = "a call that waits for itself is a bug in the caller, and a panic names it where a deadlock would hang"
)]
fn refuse_self_join() -> ! {
    std::panic::panic_any(SELF_JOIN)
}

thread_local! {
    // Each entry lasts one poll, so task migration between polls is safe.
    #[allow(clippy::disallowed_types, reason = "a per-thread list, never shared")]
    static POLLING: std::cell::Cell<Vec<usize>> = const { std::cell::Cell::new(Vec::new()) };
}

#[cfg(feature = "deterministic")]
shuttle::thread_local! {
    #[allow(clippy::disallowed_types, reason = "the list belongs to a logical task, not its OS thread")]
    static MODEL_POLLING: std::cell::Cell<Vec<usize>> = const { std::cell::Cell::new(Vec::new()) };
}

#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the list is changed within one poll of the operation"
)]
fn with_polling<R>(f: impl FnOnce(&mut Vec<usize>) -> R) -> R {
    let apply = |cell: &std::cell::Cell<Vec<usize>>| {
        let mut flights = cell.take();
        let result = f(&mut flights);
        cell.set(flights);
        result
    };
    #[cfg(feature = "deterministic")]
    if crate::runtime::execution::current().is_some() {
        return MODEL_POLLING.with(apply);
    }
    POLLING.with(apply)
}

fn address<T>(flight: &Arc<Flight<T>>) -> usize {
    Arc::as_ptr(flight).addr()
}

fn polling_here<T>(flight: &Arc<Flight<T>>) -> bool {
    let flight = address(flight);
    with_polling(|flights| flights.contains(&flight))
}

struct Polling;

impl Polling {
    fn enter<T>(flight: &Arc<Flight<T>>) -> Self {
        let flight = address(flight);
        with_polling(|flights| flights.push(flight));
        Self
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        with_polling(|flights| flights.pop());
    }
}

impl<K, T> Clone for SingleFlight<K, T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<K, T> Default for SingleFlight<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, T> fmt::Debug for SingleFlight<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingleFlight")
            .field("in_flight", &self.len())
            .finish()
    }
}
