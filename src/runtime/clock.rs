//! Execution-local timer registry and idle driver. The scheduler excludes the
//! driver while any application task is runnable. Waking from the driver task,
//! rather than the scheduler callback, avoids reentering Shuttle's state borrow.

#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "clock bookkeeping is confined to the harness OS thread"
)]

use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    panic::Location,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll, Waker},
    time::Duration,
};

use shuttle::scheduler::{Task, TaskId};
use shuttle_engine::runtime::execution::ExecutionState;
use tokio::time::Instant;

use super::{World, trace};

thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

struct Clock {
    origin: Instant,
    now: Instant,
    next_id: u64,
    timers: BTreeMap<(Instant, u64), (Waker, u64)>,
    driver: Option<(TaskId, Waker)>,
}

fn with<T>(f: impl FnOnce(&mut Clock) -> T) -> T {
    CLOCK.with_borrow_mut(|clock| {
        #[allow(
            clippy::expect_used,
            reason = "Execution initializes its clock before running tasks"
        )]
        f(clock.as_mut().expect("modeled clock requires an execution"))
    })
}

pub(crate) fn reset() {
    // Instant has no portable constant constructor. Its opaque origin is shared
    // across executions; only virtual durations influence scheduling or replay.
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(|| Instant::from_std(std::time::Instant::now()));
    CLOCK.set(Some(Clock {
        origin,
        now: origin,
        next_id: 0,
        timers: BTreeMap::new(),
        driver: None,
    }));
}

pub(crate) fn clear() {
    CLOCK.set(None);
}

pub(crate) fn now() -> Instant {
    with(|clock| clock.now)
}

pub(crate) fn elapsed() -> Duration {
    CLOCK.with_borrow(|clock| {
        clock.as_ref().map_or(Duration::ZERO, |clock| {
            clock.now.saturating_duration_since(clock.origin)
        })
    })
}

pub(crate) fn deadline_after(duration: Duration) -> Instant {
    let now = now();
    let mut delay = duration;
    loop {
        if let Some(deadline) = now.checked_add(delay) {
            return deadline;
        }
        // An execution can advance near Instant's platform limit. Keep even
        // the fallback representable there; zero always fits at the limit.
        delay /= 2;
    }
}

pub(crate) fn start() {
    // Register directly so the scheduler knows the driver's ID before its first
    // poll. spawn_future switches before creation, never after adding the task.
    let stack_size = ExecutionState::with(|state| state.config.stack_size);
    let id = ExecutionState::spawn_future(
        Driver,
        stack_size,
        Some("adamantium clock".into()),
        Location::caller(),
    );
    let waker = ExecutionState::with(|state| {
        state.detach(id);
        state.get(id).waker()
    });
    with(|clock| clock.driver = Some((id, waker)));
}

pub(crate) fn runnable<'a>(tasks: &[&'a Task]) -> Vec<&'a Task> {
    let driver = with(|clock| clock.driver.as_ref().map(|(id, _)| *id));
    let application = tasks
        .iter()
        .copied()
        .filter(|task| Some(task.id()) != driver)
        .collect::<Vec<_>>();
    // Spurious wake candidates are not runnable work and must not keep time
    // frozen. They remain candidates whenever actual application work exists.
    if application.iter().any(|task| task.runnable()) {
        application
    } else {
        tasks
            .iter()
            .copied()
            .filter(|task| Some(task.id()) == driver)
            .collect()
    }
}

struct Driver;

impl Future for Driver {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let due = with(|clock| {
            let Some((&(deadline, _), _)) = clock.timers.first_key_value() else {
                return Vec::new();
            };
            clock.now = deadline;
            let mut due = Vec::new();
            while clock
                .timers
                .first_key_value()
                .is_some_and(|(&(at, _), _)| at <= deadline)
            {
                if let Some((_, entry)) = clock.timers.pop_first() {
                    due.push(entry);
                }
            }
            due
        });
        if !due.is_empty() {
            for (waker, wait_id) in due {
                trace::record_wakeup("time.sleep", wait_id, true);
                waker.wake();
            }
            cx.waker().wake_by_ref();
        }
        // With no timers, park: the engine must still diagnose a real deadlock.
        Poll::Pending
    }
}

pub(crate) struct Sleep {
    deadline: Instant,
    key: Option<(Instant, u64)>,
    wait_id: u64,
    trace_token: Option<u64>,
    world: World,
}

impl Sleep {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            key: None,
            wait_id: 0,
            trace_token: None,
            world: World::current(),
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.world.step();
        let ready = with(|clock| {
            if self.deadline <= clock.now {
                if let Some(key) = self.key.take() {
                    clock.timers.remove(&key);
                }
                return true;
            }
            let deadline = self.deadline;
            let key = *self.key.get_or_insert_with(|| {
                let id = clock.next_id;
                #[allow(
                    clippy::expect_used,
                    reason = "an execution cannot allocate 2^64 timers"
                )]
                let next_id = id.checked_add(1).expect("modeled timer identity exhausted");
                clock.next_id = next_id;
                (deadline, id)
            });
            if self.wait_id == 0 {
                self.wait_id = trace::alloc_wait_id();
            }
            clock.timers.insert(key, (cx.waker().clone(), self.wait_id));
            if let Some((_, driver)) = &clock.driver {
                driver.wake_by_ref();
            }
            false
        });
        if ready {
            if let Some(token) = self.trace_token.take() {
                trace::end_wait(token, false);
            }
            return Poll::Ready(());
        }
        if self.trace_token.is_none() {
            self.trace_token = trace::begin_wait("time.sleep", self.wait_id);
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if self.world != World::current() {
            if !std::thread::panicking() {
                self.world.check();
            }
            return;
        }
        self.world.step_unless_panicking();
        if let Some(token) = self.trace_token.take() {
            trace::end_wait(token, true);
        }
        if let Some(key) = self.key.take() {
            with(|clock| {
                clock.timers.remove(&key);
            });
        }
    }
}
