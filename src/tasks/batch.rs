use std::{future::Future, sync::Arc};

use super::{Tasks, join_next, locked, no_result};
use crate::runtime::World;

/// Runs `f` on every item, at most `limit` at a time, and returns the
/// results in input order, whatever order the items finish in. A `limit` of
/// zero means one. The first error to happen ends the whole operation, even
/// with earlier items still pending: no further item is taken, the running
/// items are aborted and drained, then the error is returned. A panic in an
/// item's future does the same and is resumed here.
///
/// Apart from calling `into_iter`, nothing happens until the returned future
/// is first polled, and a future that is never polled never calls `f`. Each
/// poll takes an item and calls `f` only while fewer than `limit` items are
/// unfinished, so at most `limit` of `f`'s futures exist at once however long
/// the input is. Finished results are held until the end, so they still take
/// memory in proportion to the items completed.
///
/// The iterator and `f` live in the returned future, which is why they must
/// be `Send`; `I` and the items need not be. Both run inside the future's
/// poll rather than in a task, so a slow iterator or `f` holds up whoever is
/// awaiting, including noticing that an item failed: put slow work in the
/// future `f` returns. A panic in the iterator or in `f` itself unwinds
/// through the poll and aborts the running items without waiting for them.
///
/// Each item runs as a task in its own scope, so polling needs a Tokio
/// runtime, and panics at the first item without one.
pub fn concurrently<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    mut f: F,
) -> impl Future<Output = Result<Vec<T>, E>> + Send
where
    I: IntoIterator,
    I::IntoIter: Send,
    F: FnMut(I::Item) -> Fut + Send,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    // Stop reading the iterator after None. It may resume or panic.
    let mut items = items.into_iter().fuse();
    let limit = limit.max(1);
    crate::runtime::in_world(World::current(), async move {
        let mut tasks = Tasks::new();
        // Tasks report here rather than through handles, so items that finish
        // behind a slow one leave only their results behind.
        let finished = locked(Vec::new());
        let mut started = 0;
        // Fill the window, then wait for a place to open. A task records its
        // error just before it ends, so this stops before taking another item.
        while !tasks.failed() {
            if tasks.spawner.set.lock("JoinSet::len").len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let finished = Arc::clone(&finished);
                tasks.spawn(async move {
                    let value = future.await?;
                    finished.lock("Tasks::record_finished").push((index, value));
                    Ok(())
                });
                continue;
            }
            match join_next(&tasks.spawner.set).await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => {
                    // Nothing here has aborted a task, so a cancellation means
                    // its runtime shut down, and that item will have no result.
                    tasks.remember_panic(join_error);
                    tasks.abort();
                    break;
                }
                None => break,
            }
        }
        tasks.join(Ok(())).await?;
        let finished = std::mem::take(&mut *finished.lock("Tasks::take_finished"));
        if finished.len() != started {
            no_result();
        }
        let mut values = in_input_order(finished);
        values.shrink_to_fit();
        Ok(values)
    })
}

fn in_input_order<V>(mut indexed: Vec<(usize, V)>) -> Vec<V> {
    indexed.sort_unstable_by_key(|(index, _)| *index);
    indexed.into_iter().map(|(_, value)| value).collect()
}

struct Outcomes<T, E> {
    winner: Option<T>,
    errors: Vec<(usize, E)>,
}

/// Runs `f` on items, at most `limit` at a time, and returns the first
/// success to happen. A `limit` of zero means one. With a limit of one this
/// tries the items in turn until one succeeds; with a limit as long as the
/// input it races them all.
///
/// A failure only frees a place for the next item. The first success ends
/// the whole operation: no further item is taken, the running items are
/// aborted and drained, then the value is returned. A success that lands
/// while they drain is dropped. If every item fails, all of their errors
/// come back in input order, so the error at index `i` is item `i`'s; an
/// empty input fails with no errors. A panic in an item's future aborts and
/// drains the others and is resumed here, even after a success.
///
/// Laziness, the `Send` bounds, where the iterator and `f` run, and the need
/// for a Tokio runtime are as for [`concurrently`].
///
/// ```
/// use adamantium::first_ok;
/// # async fn fetch(mirror: &str) -> Result<String, String> {
/// #     if mirror == "a.example.com" { Err(format!("{mirror} is down")) } else { Ok(mirror.to_owned()) }
/// # }
/// # #[tokio::main] async fn main() -> Result<(), Vec<String>> {
/// let mirrors = ["a.example.com", "b.example.com", "c.example.com"];
/// let page = first_ok(mirrors, 1, fetch).await?;
/// assert_eq!(page, "b.example.com");
/// # Ok(()) }
/// ```
pub fn first_ok<I, F, Fut, T, E>(
    items: I,
    limit: usize,
    mut f: F,
) -> impl Future<Output = Result<T, Vec<E>>> + Send
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
    crate::runtime::in_world(World::current(), async move {
        // Items report here rather than as task errors, which would make the
        // scope abort the others at the first failure.
        let mut tasks = Tasks::<std::convert::Infallible>::new();
        let outcomes = locked(Outcomes {
            winner: None,
            errors: Vec::new(),
        });
        let mut started = 0;
        // A task reports just before it ends, so this stops before taking
        // another item once one has succeeded.
        while outcomes.lock("Tasks::race").winner.is_none() && !tasks.failed() {
            if tasks.spawner.set.lock("JoinSet::len").len() < limit
                && let Some(item) = items.next()
            {
                let index = started;
                started += 1;
                let future = f(item);
                let outcomes = Arc::clone(&outcomes);
                tasks.spawn(async move {
                    let result = future.await;
                    let mut outcomes = outcomes.lock("Tasks::race");
                    match result {
                        Ok(value) => {
                            if outcomes.winner.is_none() {
                                outcomes.winner = Some(value);
                            }
                        }
                        Err(error) => outcomes.errors.push((index, error)),
                    }
                    Ok(())
                });
                continue;
            }
            match join_next(&tasks.spawner.set).await {
                Some(Ok(())) => {}
                Some(Err(join_error)) => {
                    // Nothing here has aborted a task yet, so a cancellation
                    // means its runtime shut down, and that item has no result.
                    tasks.remember_panic(join_error);
                    break;
                }
                None => break,
            }
        }
        tasks.abort();
        let Ok(()) = tasks.join(Ok(())).await;
        let (winner, errors) = {
            let mut outcomes = outcomes.lock("Tasks::race");
            (outcomes.winner.take(), std::mem::take(&mut outcomes.errors))
        };
        if let Some(value) = winner {
            return Ok(value);
        }
        if errors.len() != started {
            no_result();
        }
        Err(in_input_order(errors))
    })
}
