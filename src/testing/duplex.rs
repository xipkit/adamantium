//! Paired virtual byte streams for deterministic protocol and transport-fault tests.
//!
//! Built on the crate's modeled synchronization primitives ([`Lock`], [`WaitQueue`])
//! and virtual clock ([`crate::time`]), so every lock acquisition, wakeup and
//! delivery latency goes through the `adamantium::testing` scheduling seam without
//! sockets or wall-clock waits.
//!
//! # Buffering, backpressure and wakeups
//!
//! Each direction (`A -> B` and `B -> A`) is an independent bounded FIFO byte pipe
//! with capacity `buffer_capacity` (nonzero). Both ready-to-read bytes and
//! latency-delayed in-flight bytes count toward `buffer_capacity`.
//!
//! - **Partial writes and backpressure**: `poll_write` accepts up to
//!   `min(src.len(), available_space, max_write_chunk)` bytes. When
//!   `available_space == 0`, `poll_write` registers a writer wakeup and returns
//!   `Poll::Pending` until the peer drains bytes or closes/faults the link.
//! - **Partial reads**: `poll_read` delivers up to
//!   `min(ready.len(), dst.remaining(), max_read_chunk, remaining_disconnect_budget)`
//!   bytes per poll.
//! - **Clock-driven latency**: When `latency > Duration::ZERO`, each write is
//!   enqueued in-flight with `ready_at = now + latency`. A reader facing an empty
//!   ready buffer with in-flight data waits on both the direction's readability
//!   queue and `crate::time::sleep_until(ready_at)`, so virtual time advances
//!   deterministically when no task is runnable while immediate faults still wake
//!   the reader without waiting for the timer.
//! - **Wakeups**: Writing notifies the reader's queue; reading notifies the
//!   writer's queue as buffer space opens; closing, dropping, or injecting a
//!   [`TransportFault`] notifies all waiters on both sides.
//! - **EOF and mid-frame disconnect**: `poll_shutdown` (or dropping the stream)
//!   closes the outbound direction cleanly after all already-written bytes are
//!   delivered, yielding `Ok(0)` (EOF) to the peer. `disconnect_after` or
//!   [`DuplexControl::disconnect`] injects either clean EOF or an abrupt I/O
//!   error (such as `ConnectionReset`) mid-transfer.

use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::Instant,
};

use crate::runtime::{
    Guard, Lock, World,
    wait::{WaitQueue, Waiter},
};

/// Fault injected on a virtual link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFault {
    /// Clean end-of-file (`Ok(0)` on read, `BrokenPipe` on write).
    Eof,
    /// Abrupt transport error (for example [`io::ErrorKind::ConnectionReset`]).
    Error(io::ErrorKind),
}

/// Configuration for one unidirectional byte pipe (`A -> B` or `B -> A`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkConfig {
    /// Maximum buffered bytes in this direction, including in-flight bytes.
    /// Must be nonzero.
    pub buffer_capacity: usize,
    /// Optional cap on bytes delivered per `poll_read`. Must be nonzero when `Some`.
    pub max_read_chunk: Option<usize>,
    /// Optional cap on bytes accepted per `poll_write`. Must be nonzero when `Some`.
    pub max_write_chunk: Option<usize>,
    /// Virtual delivery latency applied to each write via [`crate::time`].
    pub latency: Duration,
    /// Optional byte budget after which the link terminates with [`TransportFault`].
    /// The reader receives at most `limit` bytes before observing the fault.
    pub disconnect_after: Option<(usize, TransportFault)>,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 4096,
            max_read_chunk: None,
            max_write_chunk: None,
            latency: Duration::ZERO,
            disconnect_after: None,
        }
    }
}

impl LinkConfig {
    fn validate(&self) {
        assert!(
            self.buffer_capacity > 0,
            "adamantium::testing::LinkConfig::buffer_capacity must be nonzero"
        );
        if let Some(chunk) = self.max_read_chunk {
            assert!(
                chunk > 0,
                "adamantium::testing::LinkConfig::max_read_chunk must be nonzero"
            );
        }
        if let Some(chunk) = self.max_write_chunk {
            assert!(
                chunk > 0,
                "adamantium::testing::LinkConfig::max_write_chunk must be nonzero"
            );
        }
    }
}

/// Configuration for a paired [`DuplexStream`] connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DuplexConfig {
    /// Configuration for bytes flowing from stream A to stream B.
    pub a_to_b: LinkConfig,
    /// Configuration for bytes flowing from stream B to stream A.
    pub b_to_a: LinkConfig,
}

impl DuplexConfig {
    /// Uses the same [`LinkConfig`] for both directions.
    pub fn symmetric(link: LinkConfig) -> Self {
        Self {
            a_to_b: link,
            b_to_a: link,
        }
    }

    /// Configures `A -> B` and `B -> A` independently.
    pub fn new(a_to_b: LinkConfig, b_to_a: LinkConfig) -> Self {
        Self { a_to_b, b_to_a }
    }
}

/// Creates a paired virtual full-duplex stream with `buffer_capacity` bytes per direction.
pub fn duplex(buffer_capacity: usize) -> (DuplexStream, DuplexStream) {
    duplex_with(DuplexConfig::symmetric(LinkConfig {
        buffer_capacity,
        ..LinkConfig::default()
    }))
}

/// Creates a paired virtual full-duplex stream with the given [`DuplexConfig`].
pub fn duplex_with(config: DuplexConfig) -> (DuplexStream, DuplexStream) {
    let (a, b, _) = duplex_controlled(config);
    (a, b)
}

/// Creates a paired virtual full-duplex stream along with a [`DuplexControl`]
/// handle for concurrent fault injection.
pub fn duplex_controlled(config: DuplexConfig) -> (DuplexStream, DuplexStream, DuplexControl) {
    config.a_to_b.validate();
    config.b_to_a.validate();
    let a_to_b = Arc::new(Direction::new(config.a_to_b));
    let b_to_a = Arc::new(Direction::new(config.b_to_a));
    let a = DuplexStream::new(Arc::clone(&b_to_a), Arc::clone(&a_to_b));
    let b = DuplexStream::new(Arc::clone(&a_to_b), Arc::clone(&b_to_a));
    let control = DuplexControl { a_to_b, b_to_a };
    (a, b, control)
}

/// Concurrent fault-injection and close handle for a [`DuplexStream`] pair.
#[derive(Clone)]
pub struct DuplexControl {
    a_to_b: Arc<Direction>,
    b_to_a: Arc<Direction>,
}

impl DuplexControl {
    /// Immediately applies `fault` to both directions and wakes all waiting readers and writers.
    pub fn disconnect(&self, fault: TransportFault) {
        self.fault_a_to_b(fault);
        self.fault_b_to_a(fault);
    }

    /// Immediately applies `fault` to the `A -> B` direction and wakes its waiters.
    pub fn fault_a_to_b(&self, fault: TransportFault) {
        self.a_to_b.inject_fault(fault);
    }

    /// Immediately applies `fault` to the `B -> A` direction and wakes its waiters.
    pub fn fault_b_to_a(&self, fault: TransportFault) {
        self.b_to_a.inject_fault(fault);
    }
}

struct ReadTimer {
    deadline: Instant,
    sleep: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// One endpoint of a paired virtual stream implementing [`AsyncRead`] and [`AsyncWrite`].
pub struct DuplexStream {
    inbound: Arc<Direction>,
    outbound: Arc<Direction>,
    read_waiter: Option<Waiter>,
    write_waiter: Option<Waiter>,
    read_timer: Option<ReadTimer>,
}

impl DuplexStream {
    fn new(inbound: Arc<Direction>, outbound: Arc<Direction>) -> Self {
        Self {
            inbound,
            outbound,
            read_waiter: None,
            write_waiter: None,
            read_timer: None,
        }
    }

    /// Returns the total bytes delivered so far on the inbound direction.
    pub fn bytes_read(&self) -> usize {
        self.inbound.lock().bytes_delivered
    }

    /// Returns the total bytes delivered to the peer so far on the outbound direction.
    pub fn bytes_written_delivered(&self) -> usize {
        self.outbound.lock().bytes_delivered
    }
}

impl Drop for DuplexStream {
    fn drop(&mut self) {
        if std::thread::panicking() && self.outbound.world != World::current() {
            return;
        }
        self.outbound.world.step_unless_panicking();
        {
            let mut out_state = self.outbound.state.lock();
            out_state.write_closed = true;
        }
        self.outbound.readable.notify_all();
        {
            let mut in_state = self.inbound.state.lock();
            in_state.read_closed = true;
        }
        self.inbound.writable.notify_all();
    }
}

impl AsyncRead for DuplexStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.inbound.world.check();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        loop {
            let mut state = this.inbound.lock();
            if let Some(kind) = state.fault {
                drop(state);
                this.read_waiter = None;
                this.read_timer = None;
                return Poll::Ready(Err(io::Error::from(kind)));
            }

            if let Some((limit, fault)) = this.inbound.config.disconnect_after
                && state.bytes_delivered >= limit
            {
                let result = apply_disconnect_fault(&mut state, fault);
                drop(state);
                this.read_waiter = None;
                this.read_timer = None;
                this.inbound.writable.notify_all();
                return Poll::Ready(result);
            }

            let now = crate::time::now();
            promote_due(&mut state, now);

            if !state.ready.is_empty() {
                let mut limit = state.ready.len().min(buf.remaining());
                if let Some(max_chunk) = this.inbound.config.max_read_chunk {
                    limit = limit.min(max_chunk);
                }
                if let Some((budget, _)) = this.inbound.config.disconnect_after {
                    limit = limit.min(budget.saturating_sub(state.bytes_delivered));
                }
                let copied = drain_into_read_buf(&mut state.ready, buf, limit);
                state.bytes_delivered += copied;
                drop(state);
                this.read_waiter = None;
                this.read_timer = None;
                this.inbound.writable.notify_one();
                return Poll::Ready(Ok(()));
            }

            if let Some(front) = state.in_flight.front() {
                let next_ready = front.ready_at;
                drop(state);

                if this.read_waiter.is_none() {
                    this.read_waiter = Some(this.inbound.readable.listen());
                    // Recheck under the lock in case a fault arrived before listening.
                    continue;
                }
                if this
                    .read_timer
                    .as_ref()
                    .is_none_or(|timer| timer.deadline != next_ready)
                {
                    this.read_timer = Some(ReadTimer {
                        deadline: next_ready,
                        sleep: Box::pin(crate::time::sleep_until(next_ready)),
                    });
                }

                if let Some(waiter) = &mut this.read_waiter
                    && Pin::new(waiter).poll(cx).is_ready()
                {
                    this.read_waiter = None;
                    continue;
                }
                if let Some(timer) = &mut this.read_timer
                    && timer.sleep.as_mut().poll(cx).is_ready()
                {
                    this.read_timer = None;
                    continue;
                }
                return Poll::Pending;
            }

            if state.write_closed {
                drop(state);
                this.read_waiter = None;
                this.read_timer = None;
                return Poll::Ready(Ok(()));
            }

            drop(state);
            if let Some(waiter) = &mut this.read_waiter {
                if Pin::new(waiter).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.read_waiter = None;
            } else {
                this.read_waiter = Some(this.inbound.readable.listen());
            }
        }
    }
}

impl AsyncWrite for DuplexStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        this.outbound.world.check();

        loop {
            let mut state = this.outbound.lock();
            if let Some(kind) = state.fault {
                drop(state);
                this.write_waiter = None;
                return Poll::Ready(Err(io::Error::from(kind)));
            }
            if state.write_closed || state.read_closed {
                drop(state);
                this.write_waiter = None;
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            if let Some((limit, fault)) = this.outbound.config.disconnect_after
                && state.bytes_delivered >= limit
            {
                let err = match fault {
                    TransportFault::Eof => {
                        state.write_closed = true;
                        io::ErrorKind::BrokenPipe.into()
                    }
                    TransportFault::Error(kind) => {
                        state.fault = Some(kind);
                        io::Error::from(kind)
                    }
                };
                drop(state);
                this.write_waiter = None;
                this.outbound.readable.notify_all();
                return Poll::Ready(Err(err));
            }
            if buf.is_empty() {
                drop(state);
                this.write_waiter = None;
                return Poll::Ready(Ok(0));
            }

            let occupied = state.ready.len() + state.in_flight_len;
            let available = this
                .outbound
                .config
                .buffer_capacity
                .saturating_sub(occupied);
            if available > 0 {
                let mut to_write = buf.len().min(available);
                if let Some(max_chunk) = this.outbound.config.max_write_chunk {
                    to_write = to_write.min(max_chunk);
                }
                if let Some(slice) = buf.get(..to_write) {
                    if this.outbound.config.latency.is_zero() {
                        state.ready.extend(slice);
                    } else {
                        let ready_at = delivery_deadline(this.outbound.config.latency);
                        state.in_flight_len += to_write;
                        state.in_flight.push_back(InFlight {
                            ready_at,
                            bytes: slice.iter().copied().collect(),
                        });
                    }
                }
                drop(state);
                this.write_waiter = None;
                this.outbound.readable.notify_one();
                return Poll::Ready(Ok(to_write));
            }

            drop(state);
            if let Some(waiter) = &mut this.write_waiter {
                if Pin::new(waiter).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.write_waiter = None;
            } else {
                this.write_waiter = Some(this.outbound.writable.listen());
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.outbound.world.check();
        let state = self.outbound.lock();
        if let Some(kind) = state.fault {
            return Poll::Ready(Err(io::Error::from(kind)));
        }
        if state.write_closed || state.read_closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        this.outbound.world.check();
        let mut state = this.outbound.lock();
        if let Some(kind) = state.fault {
            return Poll::Ready(Err(io::Error::from(kind)));
        }
        state.write_closed = true;
        drop(state);
        this.write_waiter = None;
        this.outbound.readable.notify_all();
        Poll::Ready(Ok(()))
    }
}

struct Direction {
    state: Lock<DirectionState>,
    readable: WaitQueue,
    writable: WaitQueue,
    world: World,
    config: LinkConfig,
}

struct InFlight {
    ready_at: Instant,
    bytes: VecDeque<u8>,
}

struct DirectionState {
    ready: VecDeque<u8>,
    in_flight: VecDeque<InFlight>,
    in_flight_len: usize,
    bytes_delivered: usize,
    write_closed: bool,
    read_closed: bool,
    fault: Option<io::ErrorKind>,
}

impl Direction {
    fn new(config: LinkConfig) -> Self {
        let world = World::current();
        Self {
            state: Lock::new(DirectionState {
                ready: VecDeque::with_capacity(config.buffer_capacity),
                in_flight: VecDeque::new(),
                in_flight_len: 0,
                bytes_delivered: 0,
                write_closed: false,
                read_closed: false,
                fault: None,
            }),
            readable: WaitQueue::named("duplex.read"),
            writable: WaitQueue::named("duplex.write"),
            world,
            config,
        }
    }

    fn lock(&self) -> Guard<'_, DirectionState> {
        self.world.step();
        self.state.lock()
    }

    fn inject_fault(&self, fault: TransportFault) {
        let mut state = self.lock();
        match fault {
            TransportFault::Eof => {
                state.write_closed = true;
            }
            TransportFault::Error(kind) => {
                state.fault = Some(kind);
                state.ready.clear();
                state.in_flight.clear();
                state.in_flight_len = 0;
            }
        }
        drop(state);
        self.readable.notify_all();
        self.writable.notify_all();
    }
}

fn apply_disconnect_fault(state: &mut DirectionState, fault: TransportFault) -> io::Result<()> {
    match fault {
        TransportFault::Eof => {
            state.write_closed = true;
            Ok(())
        }
        TransportFault::Error(kind) => {
            state.fault = Some(kind);
            Err(io::Error::from(kind))
        }
    }
}

fn promote_due(state: &mut DirectionState, now: Instant) {
    while let Some(front) = state.in_flight.front() {
        if front.ready_at <= now {
            if let Some(chunk) = state.in_flight.pop_front() {
                state.in_flight_len -= chunk.bytes.len();
                state.ready.extend(chunk.bytes);
            }
        } else {
            break;
        }
    }
}

fn drain_into_read_buf(ready: &mut VecDeque<u8>, buf: &mut ReadBuf<'_>, limit: usize) -> usize {
    let mut copied = 0;
    while copied < limit {
        let (front, _) = ready.as_slices();
        if front.is_empty() {
            break;
        }
        let take = front.len().min(limit - copied);
        if let Some(slice) = front.get(..take) {
            buf.put_slice(slice);
            ready.drain(..take);
            copied += take;
        } else {
            break;
        }
    }
    copied
}

fn delivery_deadline(latency: Duration) -> Instant {
    if World::current().is_model() {
        return crate::runtime::clock::deadline_after(latency);
    }
    let now = crate::time::now();
    let mut delay = latency.min(Duration::from_secs(86400 * 365 * 30));
    loop {
        if let Some(deadline) = now.checked_add(delay) {
            return deadline;
        }
        delay /= 2;
    }
}
