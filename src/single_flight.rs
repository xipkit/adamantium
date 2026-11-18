//! Concurrent calls for one key that share one operation.

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

/// Lets concurrent calls for the same key share one operation and its
/// output.
///
/// [`run`](Self::run) starts the operation for a key if none is in flight,
/// or joins the one that is, and every caller gets a copy of its output. An
/// error is an output like any other, so put it in a `Result`: everyone
/// sharing that operation gets the error, and the next call starts a new
/// operation. Nothing is cached once an operation finishes.
///
/// No caller owns the operation. Whichever caller is waiting polls it, so
/// dropping the caller that started it leaves the others to finish it.
/// When the last caller is dropped, the operation is dropped too, and the
/// next call starts a new one. Clones share the operations in flight.
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
/// A caller can join an operation that started before it called. That is
/// right when any output produced after the call began will do, such as
/// reading a value or refreshing a token. It is wrong when a caller needs an
/// operation that starts after its call: an acknowledgment that a write has
/// been pushed can't share a push that began before the write. Let the
/// operation report what it covered and call again when that isn't enough,
/// and the second call joins an operation that started after the first
/// ended.
///
/// Operations for one key also don't wait for each other to clean up. An
/// operation dropped by its last caller runs its destructor in that caller,
/// and anything it left to finish in the background, such as the tasks of a
/// scope it was running, may still be finishing when the next call starts a
/// new operation. When separate operations must not overlap, or every
/// caller needs an operation of its own, serialize them instead, with an
/// `AsyncActor` or a [`Limit`](crate::Limit) of one.
///
/// # Panics
///
/// If the operation panics, the panic continues in the caller that was
/// polling it, and every other caller panics too. The operation can't call
/// `run` for its own key, which would wait for itself: that call panics.
/// The check sees only a call made by the operation itself, not one made by
/// a task that the operation spawns and waits for.
pub struct SingleFlight<K, T> {
    shared: Arc<Shared<K, T>>,
}

type Operation<T> = Pin<Box<dyn Future<Output = T> + Send>>;

struct Shared<K, T> {
    /// The operations in flight. A finished, panicked or cancelled one is
    /// removed under the same lock that changes its stage, so a new call
    /// never joins it. Held only in this module's synchronous methods,
    /// which run no caller code but `K::hash`, `K::eq` and `K::clone`.
    flights: Lock<HashMap<K, Arc<Flight<T>>>>,
    world: World,
}

struct Flight<T> {
    /// Locked after `Shared::flights` when both are needed.
    state: Lock<State<T>>,
    signal: Arc<Signal>,
    /// Wakes `signal`. The operation is polled with it, whichever caller
    /// polls it.
    waker: Waker,
    world: World,
}

struct State<T> {
    stage: Stage<T>,
    /// The calls that have joined and not yet returned or been dropped.
    callers: usize,
}

enum Stage<T> {
    /// No caller is polling the operation.
    Idle(Operation<T>),
    /// A caller has taken the operation out to poll it, or to create it.
    Polling,
    /// The operation finished, and other callers have yet to take a copy
    /// of its output.
    Finished(Arc<T>),
    /// The operation panicked.
    Panicked,
    /// Every caller has left: the last took the output, or the operation
    /// was dropped unfinished.
    Ended,
}

/// What the operation's waker reaches. It holds no state of the flight, so
/// a waker the operation left behind keeps nothing else alive.
struct Signal {
    /// Set when the operation is woken, and cleared by the caller that
    /// takes it to poll it.
    woken: AtomicBool,
    /// Notified when the operation is woken or the flight ends.
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
        self.shared.lock().len()
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
        let mut flights = self.shared.lock();
        if let Some(flight) = flights.get(&key) {
            let flight = Arc::clone(flight);
            if polling_here(&flight) {
                drop(flights);
                refuse_self_join();
            }
            flight.lock().callers += 1;
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

impl<K, T> Shared<K, T> {
    /// Every operation locks, so this is where a handle carried into another
    /// harness execution is rejected. While unwinding the check is skipped,
    /// so that a dropped call can still leave its flight.
    fn lock(&self) -> Guard<'_, HashMap<K, Arc<Flight<T>>>> {
        if !std::thread::panicking() {
            self.world.check();
        }
        self.flights.lock()
    }
}

impl<T> Flight<T> {
    /// A flight whose creator holds the operation it is about to create.
    fn new(world: World) -> Self {
        let signal = Arc::new(Signal {
            woken: AtomicBool::new(false),
            changed: WaitQueue::new(),
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

    fn lock(&self) -> Guard<'_, State<T>> {
        if !std::thread::panicking() {
            self.world.check();
        }
        self.state.lock()
    }
}

/// One call's membership in a flight. Dropping it before the call returns
/// leaves the flight.
struct Caller<K: Hash + Eq, T> {
    shared: Arc<Shared<K, T>>,
    key: K,
    flight: Arc<Flight<T>>,
    /// Whether this call has taken the operation out of the flight, to
    /// create or poll it.
    polling: bool,
    /// Whether this call has left the flight.
    done: bool,
}

impl<K: Hash + Eq, T: Clone> Caller<K, T> {
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        held: &mut Option<Operation<T>>,
        waiter: &mut Option<Waiter>,
    ) -> Poll<T> {
        loop {
            let mut operation = match held.take() {
                Some(operation) => operation,
                None => match self.take_or_collect(waiter) {
                    Taken::Operation(operation) => operation,
                    Taken::Output(output) => return Poll::Ready(output),
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
            match polled {
                Poll::Ready(output) => return Poll::Ready(self.finish(output)),
                Poll::Pending => {
                    self.flight.lock().stage = Stage::Idle(operation);
                    self.polling = false;
                }
            }
        }
    }

    /// Takes the operation if it was woken and no one else is polling it,
    /// or this call's copy of the output if it has finished.
    fn take_or_collect(&mut self, waiter: &mut Option<Waiter>) -> Taken<T> {
        // Listen before looking, so a wake or an ending after the look is
        // not missed.
        if waiter.is_none() {
            *waiter = Some(self.flight.signal.changed.listen());
        }
        let mut state = self.flight.lock();
        match &mut state.stage {
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

    /// Ends the flight with the operation's output and leaves it.
    fn finish(&mut self, output: T) -> T {
        let mut flights = self.shared.lock();
        let mut state = self.flight.lock();
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

    /// Ends the flight because the operation panicked while this call was
    /// creating or polling it, and leaves it.
    fn abandon(&mut self) {
        let mut flights = self.shared.lock();
        let mut state = self.flight.lock();
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
    /// Removes this call's flight from the operations in flight, unless a
    /// newer flight has already replaced it.
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
    Nothing,
}

impl<K: Hash + Eq, T> Drop for Caller<K, T> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut flights = self.shared.lock();
        let mut state = self.flight.lock();
        state.callers -= 1;
        if state.callers > 0 {
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
        // The operation's destructor, or the unclaimed output's, is the
        // caller's code, so it runs after the locks are released. A new
        // call can start a new operation in the meantime.
        drop(stage);
    }
}

/// Ends the flight as panicked if the operation panics while this call is
/// creating or polling it, so the other callers don't wait for it.
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
    /// The flights whose operations the current thread is polling, by
    /// address, innermost last. Each entry lasts one poll, so the list stays
    /// correct when a task moves to another thread between polls.
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

/// Marks the thread as polling a flight's operation until dropped,
/// including by a panic.
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
