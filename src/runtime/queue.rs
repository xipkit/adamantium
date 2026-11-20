use concurrent_queue::{ConcurrentQueue, PopError, PushError};

#[cfg(feature = "deterministic")]
use super::Lock;
use super::World;
#[cfg(feature = "deterministic")]
use std::collections::VecDeque;

pub(crate) struct Bounded<T> {
    inner: Storage<T>,
    world: World,
}

enum Storage<T> {
    Production(ConcurrentQueue<T>),
    #[cfg(feature = "deterministic")]
    Model {
        state: Lock<State<T>>,
        capacity: usize,
    },
}

#[cfg(feature = "deterministic")]
struct State<T> {
    values: VecDeque<T>,
    closed: bool,
}

impl<T> Bounded<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "bounded storage requires positive capacity");
        #[cfg(feature = "deterministic")]
        if super::execution::current().is_some() {
            return Self {
                inner: Storage::Model {
                    state: Lock::new(State {
                        values: VecDeque::new(),
                        closed: false,
                    }),
                    capacity,
                },
                world: World::current(),
            };
        }
        Self {
            inner: Storage::Production(ConcurrentQueue::bounded(capacity)),
            world: World::current(),
        }
    }

    pub(crate) fn push(&self, value: T) -> Result<(), PushError<T>> {
        self.world.step();
        match &self.inner {
            Storage::Production(queue) => queue.push(value),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, capacity } => {
                let mut state = state.lock();
                if state.closed {
                    Err(PushError::Closed(value))
                } else if state.values.len() == *capacity {
                    Err(PushError::Full(value))
                } else {
                    state.values.push_back(value);
                    Ok(())
                }
            }
        }
    }

    pub(crate) fn pop(&self) -> Result<T, PopError> {
        self.world.step();
        match &self.inner {
            Storage::Production(queue) => queue.pop(),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, .. } => {
                let mut state = state.lock();
                if let Some(value) = state.values.pop_front() {
                    Ok(value)
                } else if state.closed {
                    Err(PopError::Closed)
                } else {
                    Err(PopError::Empty)
                }
            }
        }
    }

    pub(crate) fn close(&self) -> bool {
        self.world.step_unless_panicking();
        match &self.inner {
            Storage::Production(queue) => queue.close(),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, .. } => !std::mem::replace(&mut state.lock().closed, true),
        }
    }

    // Drops every queued value, oldest first. Each value is taken out of the
    // storage before it drops, so its destructor holds no storage lock and
    // may use the same queue. If a destructor panics, the rest still drop,
    // as they would in a `Vec`. Like `close`, takes no step while unwinding.
    pub(crate) fn clear(&self) {
        struct Rest<'a, T> {
            queue: &'a Bounded<T>,
            done: bool,
        }

        impl<T> Drop for Rest<'_, T> {
            fn drop(&mut self) {
                if !self.done {
                    self.queue.clear();
                }
            }
        }

        let mut rest = Rest {
            queue: self,
            done: false,
        };
        while let Some(value) = self.take() {
            drop(value);
        }
        rest.done = true;
    }

    fn take(&self) -> Option<T> {
        self.world.step_unless_panicking();
        match &self.inner {
            Storage::Production(queue) => queue.pop().ok(),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, .. } => state.lock().values.pop_front(),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.world.step();
        match &self.inner {
            Storage::Production(queue) => queue.is_closed(),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, .. } => state.lock().closed,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.world.step();
        match &self.inner {
            Storage::Production(queue) => queue.len(),
            #[cfg(feature = "deterministic")]
            Storage::Model { state, .. } => state.lock().values.len(),
        }
    }
}
