use std::{
    collections::{BTreeMap, VecDeque},
    future,
    sync::Arc,
};

use futures_core::Stream;

use super::{Cancellation, ScopeError, Tasks, any_recorded, lock, no_result, shared};

/// Maps items concurrently and yields successful results in input order.
///
/// At most `max(limit, 1)` items are started but not yet yielded. This includes
/// running futures and buffered results: a slow first item holds up refill,
/// rather than allowing an unbounded queue behind it. Memory used by the map
/// is O(limit), excluding the input iterator, individual items/results, and
/// results the consumer keeps. Nothing is prefetched outside this window.
///
/// Apart from `into_iter`, work starts only when polled. Pausing consumption
/// stops scheduling; already started tasks can finish into the bounded buffer.
/// The iterator and `f` run synchronously in the consumer's poll and must be
/// `Send`, but items need not be. Put slow work in the returned future. Polling
/// the first nonempty window requires a Tokio runtime.
///
/// An observed error stops intake, aborts and drains outstanding async tasks,
/// discards buffered successes, and yields one `ScopeError::Failed`, then ends.
/// Errors are fail-fast even behind an earlier pending item. A child panic wins
/// over errors and is resumed after draining; otherwise the oldest recorded
/// error wins. Successes already yielded cannot be retracted. Unexpected task
/// cancellation (such as runtime shutdown) also drains, then panics if no child
/// panic or error explains the missing result.
///
/// Dropping the stream requests aborts without waiting. Dropping a pending
/// `next()` future while retaining the stream preserves its work and results.
/// Panics in the iterator or `f` itself abort without draining. Async cleanup
/// is not guaranteed; detached work, including a started [`super::blocking`]
/// closure, can outlive the stream. These maps register only async tasks.
///
/// All variants yield `Result<T, ScopeError<E>>`, so consumers can switch to
/// [`concurrently_ordered_until`] without changing their error handling.
/// The stream is not `Unpin`; pin it before using `StreamExt::next`.
///
/// ```
/// use adamantium::concurrently_ordered;
/// use futures_util::StreamExt;
/// # #[tokio::main] async fn main() {
/// let results = concurrently_ordered(0..4, 2, |n| async move {
///     Ok::<_, std::io::Error>(n * 2)
/// });
/// let mut results = std::pin::pin!(results);
/// let mut values = Vec::new();
/// while let Some(result) = results.next().await {
///     values.push(result.unwrap());
/// }
/// assert_eq!(values, [0, 2, 4, 6]);
/// # }
/// ```
pub fn concurrently_ordered<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    f: F,
) -> impl Stream<Item = Result<T, ScopeError<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    concurrently_ordered_until(items, limit, Cancellation::new(), f)
}

/// Maps items concurrently and yields results in observed completion order.
///
/// Simultaneous completions have unspecified order. The window, backpressure,
/// error, panic, runtime, and drop contracts are those of
/// [`concurrently_ordered`]; this variant can yield a later success while an
/// earlier item is still pending.
pub fn concurrently_unordered<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    f: F,
) -> impl Stream<Item = Result<T, ScopeError<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    concurrently_unordered_until(items, limit, Cancellation::new(), f)
}

/// An ordered map that also observes an explicit cancellation request.
///
/// The supplied signal is an observe-only parent: finishing, failing, panicking,
/// or dropping this stream never cancels it or sibling streams. Unlike
/// [`Tasks::run_until`], the stream does not register it as a scope-owned signal.
/// Futures can capture clones to observe the parent's shutdown request.
///
/// A request present at a poll wins over scheduling or yielding more successes.
/// Outstanding tasks are aborted and drained before one terminal error is
/// yielded. A child panic wins over the oldest recorded child error, which wins
/// over `ScopeError::Cancelled`, including failures recorded during cleanup.
/// A request made while the consumer is paused takes effect on its next poll;
/// no background supervisor schedules or drains work. To await cleanup, cancel
/// and keep polling to the terminal error; dropping requests abort without
/// waiting. Cancellation before the first poll takes no items and calls no `f`.
/// See [`concurrently_ordered`] for the remaining contract.
pub fn concurrently_ordered_until<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    cancellation: Cancellation,
    f: F,
) -> impl Stream<Item = Result<T, ScopeError<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    map(
        items,
        limit,
        cancellation,
        f,
        Results::Ordered(Default::default()),
    )
}

/// A completion-order map that observes cancellation as described by
/// [`concurrently_ordered_until`], with [`concurrently_unordered`]'s ordering.
pub fn concurrently_unordered_until<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    cancellation: Cancellation,
    f: F,
) -> impl Stream<Item = Result<T, ScopeError<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    map(
        items,
        limit,
        cancellation,
        f,
        Results::Unordered(VecDeque::new()),
    )
}

enum Results<T> {
    Ordered(BTreeMap<usize, T>),
    Unordered(VecDeque<T>),
}

impl<T> Results<T> {
    fn insert(&mut self, index: usize, value: T) {
        match self {
            Self::Ordered(values) => {
                values.insert(index, value);
            }
            Self::Unordered(values) => values.push_back(value),
        }
    }

    fn contains(&self, index: usize) -> bool {
        match self {
            Self::Ordered(values) => values.contains_key(&index),
            Self::Unordered(values) => !values.is_empty(),
        }
    }

    fn take(&mut self, index: usize) -> Option<T> {
        match self {
            Self::Ordered(values) => values.remove(&index),
            Self::Unordered(values) => values.pop_front(),
        }
    }
}

fn map<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    cancellation: Cancellation,
    mut f: F,
    results: Results<T>,
) -> impl Stream<Item = Result<T, ScopeError<E>>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let mut items = items.into_iter().fuse();
    let limit = limit.max(1);
    async_stream::stream! {
        let mut tasks = Tasks::new();
        let results = shared(results);
        let mut started = 0usize;
        let mut yielded = 0usize;
        let mut outstanding = 0usize;
        let mut exhausted = false;
        let mut missing = false;
        let reason = loop {
            if any_recorded(&tasks.errors) {
                break None;
            }
            // Join readiness can lag result publication. Bound unjoined handles
            // too, even if their values have already been yielded.
            let can_start = !exhausted && outstanding < limit && tasks.len() < limit;
            let can_yield = lock(&results).contains(yielded);
            if tasks.is_empty() && !can_start && !can_yield {
                missing = outstanding != 0;
                break cancellation.is_cancelled().then_some(ScopeError::Cancelled);
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Some(ScopeError::Cancelled),
                joined = tasks.set.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = joined {
                        tasks.remember_panic(error);
                        missing = true;
                        break None;
                    }
                }
                () = future::ready(()), if can_start || can_yield => {
                    // Joining can race with a worker recording an error. Check
                    // again before consuming an input or yielding a success.
                    if any_recorded(&tasks.errors) || cancellation.is_cancelled() {
                        continue;
                    }
                    if can_start {
                        match items.next() {
                            Some(item) => {
                                let index = started;
                                // Indices may wrap on an endless input; the
                                // bounded window keeps outstanding indices unique.
                                started = started.wrapping_add(1);
                                outstanding += 1;
                                let future = f(item);
                                let results = Arc::clone(&results);
                                tasks.spawn(async move {
                                    let value = future.await?;
                                    lock(&results).insert(index, value);
                                    Ok(())
                                });
                            }
                            None => exhausted = true,
                        }
                    } else {
                        let value = lock(&results).take(yielded);
                        if let Some(value) = value {
                            yielded = yielded.wrapping_add(1);
                            outstanding -= 1;
                            yield Ok(value);
                        }
                    }
                }
            }
        };
        tasks.abort();
        let terminal = match tasks.join(Ok(())).await {
            Err(error) => Some(ScopeError::Failed(error)),
            Ok(()) if missing => no_result(),
            Ok(()) => reason,
        };
        drop(results);
        drop(items);
        drop(f);
        if let Some(error) = terminal {
            yield Err(error);
        }
    }
}
