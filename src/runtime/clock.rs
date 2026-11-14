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
};

use shuttle::scheduler::{Task, TaskId};
use shuttle_engine::runtime::execution::ExecutionState;
use tokio::time::Instant;

use super::World;

thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

struct Clock {
    now: Instant,
    next_id: u64,
    timers: BTreeMap<(Instant, u64), Waker>,
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
                if let Some((_, waker)) = clock.timers.pop_first() {
                    due.push(waker);
                }
            }
            due
        });
        if !due.is_empty() {
            for waker in due {
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
    world: World,
}

impl Sleep {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            key: None,
            world: World::current(),
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.world.step();
        with(|clock| {
            if self.deadline <= clock.now {
                if let Some(key) = self.key.take() {
                    clock.timers.remove(&key);
                }
                return Poll::Ready(());
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
            clock.timers.insert(key, cx.waker().clone());
            if let Some((_, driver)) = &clock.driver {
                driver.wake_by_ref();
            }
            Poll::Pending
        })
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
        if let Some(key) = self.key.take() {
            with(|clock| {
                clock.timers.remove(&key);
            });
        }
    }
}
