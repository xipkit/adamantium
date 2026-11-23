# Deterministic tests for application code

Concurrent code fails in the interleavings that an ordinary test run happens
to miss. `adamantium::testing` runs a test's async code many times and makes
every scheduling decision itself. At each `Actor` call, channel or watch
operation, timer and drop, it picks the task that runs next. When an execution
fails, it prints the failing schedule as a record that replays exactly, and a
timeline of which task ran which operation. Time is virtual, so a test of a
30-second timeout takes no real time.

This guide tests three pieces of application code: a type that holds shared
state, a framed network protocol, and token expiry. Then it covers running the
tests in CI, what has to stay outside the harness, and how to choose a
scheduler. The [reference](#reference) at the end lists what the harness
models.

## Set up

Enable the `deterministic` feature for tests only:

```toml
[dependencies]
adamantium = "0.4"

[dev-dependencies]
adamantium = { version = "0.4", features = ["deterministic"] }
```

The harness takes over only inside `testing::check` and the other `testing`
entry points. Elsewhere in the same test build, application code and Tokio
tests use the production primitives.

Every code block in this guide runs as a doctest of the library. A doctest has
no `#[test]` function, so the examples call `testing::check` at the top level.
In a crate, put each call in a `#[test]` function. Use `#[test]`, not
`#[tokio::test]`: `check` runs the futures itself and panics if a Tokio runtime
is running.

## Test a type that holds shared state

The first example is a hub that sends project changes to the clients watching
each project. It has the shape of the `Hub` in Kan, a web app built on
Adamantium. An `Actor` holds a table of projects. Each project has a `Watch`
with its latest change and a sequence number, so a client that falls behind
can tell that it missed something. A `Subscription` is one client's watch.
Dropping it removes the project from the table once its last watcher leaves.

```rust
use std::{collections::BTreeMap, future::Future, pin::Pin};

use adamantium::{
    Actor, Watch, Watcher,
    testing::{self, Config},
};

/// A change to a project, sent to every client watching it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectEvent {
    Changed { actor: String },
    /// The client missed changes and should reload the project.
    Resync,
}

#[derive(Default)]
struct HubState {
    projects: BTreeMap<u32, Project>,
    closed: bool,
}

struct Project {
    seq: u64,
    watchers: usize,
    watch: Watch<(u64, Option<ProjectEvent>)>,
}

/// Sends project changes to the clients watching each project.
#[derive(Clone, Default)]
pub struct Hub {
    state: Actor<HubState>,
}

/// One client's watch on a project.
pub struct Subscription {
    hub: Actor<HubState>,
    project: u32,
    watcher: Watcher<(u64, Option<ProjectEvent>)>,
    last_seq: u64,
}

impl Hub {
    pub fn subscribe(&self, project: u32) -> Subscription {
        let (watcher, last_seq) = self.state.with(|state| {
            if state.closed {
                let watch = Watch::new((0, None));
                watch.close();
                return (watch.subscribe(), 0);
            }
            let entry = state.projects.entry(project).or_insert_with(|| Project {
                seq: 0,
                watchers: 0,
                watch: Watch::new((0, None)),
            });
            entry.watchers += 1;
            (entry.watch.subscribe(), entry.seq)
        });
        Subscription { hub: self.state.clone(), project, watcher, last_seq }
    }

    /// Tells everyone watching `project` that it changed.
    pub fn publish(&self, project: u32, event: ProjectEvent) {
        self.state.with(|state| {
            if let Some(entry) = state.projects.get_mut(&project) {
                entry.seq += 1;
                // A closed watch has nobody left to tell.
                drop(entry.watch.send((entry.seq, Some(event))));
            }
        });
    }

    /// Closes every project's watch, so each client's loop ends.
    pub fn close(&self) {
        self.state.with(|state| {
            state.closed = true;
            for project in std::mem::take(&mut state.projects).into_values() {
                project.watch.close();
            }
        });
    }

    /// The number of projects with at least one watcher.
    pub fn project_count(&self) -> usize {
        self.state.read(|state| state.projects.len())
    }
}

impl Subscription {
    /// Waits for the next change. Changes the client missed arrive as one
    /// `Resync`. Returns `None` once the hub closes.
    pub async fn recv(&mut self) -> Option<ProjectEvent> {
        let (seq, event) = self.watcher.receive().await?;
        let expected = self.last_seq + 1;
        self.last_seq = seq;
        match event {
            Some(event) if seq == expected => Some(event),
            _ => Some(ProjectEvent::Resync),
        }
    }
}

impl Drop for Subscription {
    /// Forgets the project once its last watcher leaves.
    fn drop(&mut self) {
        self.hub.with(|state| {
            if let Some(entry) = state.projects.get_mut(&self.project) {
                entry.watchers -= 1;
                if entry.watchers == 0 {
                    state.projects.remove(&self.project);
                }
            }
        });
    }
}

/// Two clients watch project 7 and leave at the same time. Afterwards the
/// hub must have forgotten the project.
async fn two_watchers_leave() {
    let hub = Hub::default();
    let leavers = (0..2).map(|_| {
        let hub = hub.clone();
        async move { drop(hub.subscribe(7)) }
    });
    testing::parallel(leavers).await;
    assert_eq!(hub.project_count(), 0, "a project outlived its watchers");
}

type Client = Pin<Box<dyn Future<Output = Vec<ProjectEvent>> + Send>>;

/// One client watches project 7 while two others change it, then the hub
/// closes. The watcher must end up knowing about the last change: it saw both
/// changes in order, or its last event is a `Resync`.
async fn watcher_learns_of_the_last_change() {
    let hub = Hub::default();
    let mut subscription = hub.subscribe(7);
    let watcher: Client = Box::pin(async move {
        let mut seen = Vec::new();
        while let Some(event) = subscription.recv().await {
            seen.push(event);
        }
        seen
    });
    let writers: Client = Box::pin(async move {
        let writes = ["u1", "u2"].map(|actor| {
            let hub = hub.clone();
            async move { hub.publish(7, ProjectEvent::Changed { actor: actor.to_owned() }) }
        });
        testing::parallel(writes).await;
        hub.close();
        Vec::new()
    });
    let [seen, _] = testing::parallel([watcher, writers]).await.try_into().unwrap();
    let up_to_date = match seen.as_slice() {
        [ProjectEvent::Changed { .. }, ProjectEvent::Changed { .. }] => true,
        [.., ProjectEvent::Resync] => true,
        _ => false,
    };
    assert!(up_to_date, "the watcher missed the last change: {seen:?}");
}

testing::check(Config::default(), two_watchers_leave).unwrap();
testing::check(Config::default(), watcher_learns_of_the_last_change).unwrap();
```

Each scenario is an async function. `check` calls it once per execution, so
every execution starts with fresh state. `testing::parallel` runs each future
as its own task and returns their outputs in order. The invariant is an
ordinary assertion. `check` returns the `Replay` record of the last execution
when every execution passes, and a `Failure` for the first one that fails.

Every `Actor` call, watch operation, clone and drop is a point where the
harness can switch tasks. `two_watchers_leave` has no `await` between the
calls it races: the two drops make their `Actor` calls in an order that
differs from one execution to the next.

### Find a bug

Here is the hub again, trimmed to what one scenario uses, with one change in
`Subscription::drop`. It reads the watcher count in one `Actor` call and
writes it back in a second. Each call holds the lock, but another task can
run between the two.

```rust
use std::collections::BTreeMap;

use adamantium::{
    Actor, Watch, Watcher,
    testing::{self, Config, CorpusError, FailureKind, Replay},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectEvent {
    Changed { actor: String },
    Resync,
}

#[derive(Default)]
struct HubState {
    projects: BTreeMap<u32, Project>,
    closed: bool,
}

struct Project {
    seq: u64,
    watchers: usize,
    watch: Watch<(u64, Option<ProjectEvent>)>,
}

#[derive(Clone, Default)]
pub struct Hub {
    state: Actor<HubState>,
}

pub struct Subscription {
    hub: Actor<HubState>,
    project: u32,
    watcher: Watcher<(u64, Option<ProjectEvent>)>,
    last_seq: u64,
}

impl Hub {
    pub fn subscribe(&self, project: u32) -> Subscription {
        let (watcher, last_seq) = self.state.with(|state| {
            if state.closed {
                let watch = Watch::new((0, None));
                watch.close();
                return (watch.subscribe(), 0);
            }
            let entry = state.projects.entry(project).or_insert_with(|| Project {
                seq: 0,
                watchers: 0,
                watch: Watch::new((0, None)),
            });
            entry.watchers += 1;
            (entry.watch.subscribe(), entry.seq)
        });
        Subscription { hub: self.state.clone(), project, watcher, last_seq }
    }

    pub fn project_count(&self) -> usize {
        self.state.read(|state| state.projects.len())
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // The bug: the count is read in one call and written in another.
        let left = self.hub.read(|state| {
            state.projects.get(&self.project).map(|entry| entry.watchers - 1)
        });
        self.hub.with(|state| match left {
            Some(0) => {
                state.projects.remove(&self.project);
            }
            Some(left) => {
                if let Some(entry) = state.projects.get_mut(&self.project) {
                    entry.watchers = left;
                }
            }
            None => {}
        });
    }
}

async fn two_watchers_leave() {
    let hub = Hub::default();
    let leavers = (0..2).map(|_| {
        let hub = hub.clone();
        async move { drop(hub.subscribe(7)) }
    });
    testing::parallel(leavers).await;
    assert_eq!(hub.project_count(), 0, "a project outlived its watchers");
}

let failure = testing::check(Config::default(), two_watchers_leave).unwrap_err();
assert_eq!(failure.kind(), FailureKind::Panic);

// Look for a simpler schedule that fails the same way, and print its timeline.
let shrunk = testing::shrink(failure.replay(), two_watchers_leave).unwrap();
println!("{}", testing::explain(&shrunk, two_watchers_leave));

// Keep it. A test suite would save it under tests/schedules/.
let path = std::env::temp_dir().join(format!("hub-lost-update-{}.json", std::process::id()));
shrunk.save(&path).unwrap();

// Replay that one schedule...
let loaded = Replay::load(&path).unwrap();
let replayed = testing::replay(&loaded, two_watchers_leave).unwrap_err();
assert_eq!(replayed.replay(), &shrunk);

// ...or every saved schedule for the scenario, as a regression test.
let error = testing::replay_corpus([&path], two_watchers_leave).unwrap_err();
assert!(matches!(&error, CorpusError::Execution { source, .. } if source.kind() == FailureKind::Panic));
std::fs::remove_file(&path).unwrap();
```

With the default seed, the fourth execution fails. `check` prints the failure
to standard error, and an `unwrap` of the result prints it again in its panic
message:

```text
Task failed, serializing schedule
test panicked in task 'main-thread'

thread '…' panicked at …:
assertion `left == right` failed: a project outlived its watchers
  left: 1
 right: 0
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test panicked in task 'main-thread'
failing seed:
"
3787985437048348030
"
To replay the failure, either:
    1) pass the seed to `shuttle::check_random_with_seed, or
    2) set the environment variable SHUTTLE_RANDOM_SEED to the seed and run `shuttle::check_random`.
adamantium::testing Panic: {"format":5,"engine":"shuttle=0.9.10;engine=0.1.8;std=0.1.5;schedulers=0.1.1","scheduler":"random","seed":0,"iterations":100,"iteration":4,"max_steps":10000,"wall_epoch":{"secs_since_epoch":1767225600,"nanos_since_epoch":0},"data_seed":3787985437048348030,"steps":[0,0,0,0,2,2,0,0,3,2,2,2,3,2,3,3,3,2,2,3,3,3,2,2,3,2,2,3,3,3,3,2,0,0,0,0,0]}
Timeline:
  0ns task#0 "root" scheduled (×2)
  0ns task#0 "root" Actor::clone
  0ns task#0 "root" testing::parallel
  0ns task#2 "parallel-0" scheduled
  0ns task#2 "parallel-0" Actor::with
  0ns task#0 "root" Actor::clone
  0ns task#0 "root" testing::parallel
  0ns task#3 "parallel-1" scheduled
  0ns task#2 "parallel-0" Watch::subscribe (×2)
  0ns task#2 "parallel-0" Actor::with
  0ns task#3 "parallel-1" Actor::with
  0ns task#2 "parallel-0" Actor::clone
  0ns task#3 "parallel-1" Watch::subscribe (×2)
  0ns task#3 "parallel-1" Actor::with
  0ns task#2 "parallel-0" Actor::read (×2)
  0ns task#3 "parallel-1" Actor::clone
  0ns task#3 "parallel-1" Actor::read (×2)
  0ns task#2 "parallel-0" Actor::with (×2)
  0ns task#3 "parallel-1" Actor::with
  0ns task#2 "parallel-0" Actor::drop
  0ns task#2 "parallel-0" Watcher::drop
  0ns task#3 "parallel-1" Actor::with
  0ns task#3 "parallel-1" Actor::drop
  0ns task#3 "parallel-1" Watcher::drop
  0ns task#3 "parallel-1" Actor::drop
  0ns task#2 "parallel-0" Actor::drop
  0ns task#0 "root" scheduled
  0ns task#0 "root" Actor::read (×2)
  0ns task#0 "root" Watch::drop (×2)
  0ns failed (Panic) at task#0 "root"
Rerun the same test using Config::from_env with these environment settings
  ADAMANTIUM_SEED=0 ADAMANTIUM_ITERATIONS=100
For a replay fixture, use Replay::save on failure.replay(), then load it with Replay::load and run testing::replay with the same scenario.
```

The lines before `adamantium::testing` come from elsewhere. The panic hook
prints the panic message, here the failed `assert_eq!` with its values.
Shuttle, the scheduling engine under the harness, prints the rest: `Task
failed, serializing schedule`, the task that panicked, a `failing seed`, and
advice to call `shuttle::check_random_with_seed` or to set
`SHUTTLE_RANDOM_SEED`. Ignore Shuttle's lines. That seed is the record's
internal data seed, not `Config::seed`, `check` refuses to run with
`SHUTTLE_RANDOM_SEED` set, and the Shuttle functions bypass the harness. The
pinned Shuttle release has no setting to turn them off.

Adamantium's report has four parts:

- The first line gives the failure kind and the replay record as JSON: the
  scheduler, the seed, the iteration that failed, the step limit, the
  wall-clock epoch, and the exact schedule. The record holds no values from
  the test.
- The timeline lists the execution's scheduling steps in order. Each line
  gives the virtual time, the task, and the operation the task is about to
  run. `root` is the scenario itself, and `parallel-0` and `parallel-1` are
  the tasks that `testing::parallel` started. `(×2)` stands for consecutive
  steps of one task at one operation.
- The rerun line repeats this exploration in a test that builds its `Config`
  with `Config::from_env`. See [Run the tests](#run-the-tests).
- The last line says how to keep the schedule as a fixture.

To read the timeline, find the operations the invariant depends on. Here they
are the `Actor::read` and `Actor::with` in each worker's drop:

```text
  0ns task#2 "parallel-0" Actor::read (×2)
  0ns task#3 "parallel-1" Actor::clone
  0ns task#3 "parallel-1" Actor::read (×2)
  0ns task#2 "parallel-0" Actor::with (×2)
  0ns task#3 "parallel-1" Actor::with
```

Both workers read a count of 2 before either writes, so both write back 1. The
project keeps a count of 1 with no watchers left, and the final
`project_count` reads 1.

### Shrink, replay and keep the schedule

A sampled schedule switches tasks more often than the failure needs.
`testing::shrink` replays variations of the record and keeps each one that
fails the same way with fewer steps, or as many steps and fewer task
switches, until no variation it tries is simpler. `testing::explain` replays
a record and returns its `Timeline`. Printing the timeline of the shrunk
record gives:

```text
adamantium::testing Panic (scheduler=Random, seed=0, iteration=4, max_steps=10000):
Timeline:
  0ns task#0 "root" scheduled (×2)
  0ns task#0 "root" Actor::clone
  0ns task#0 "root" testing::parallel
  0ns task#2 "parallel-0" scheduled
  0ns task#2 "parallel-0" Actor::with
  0ns task#0 "root" Actor::clone
  0ns task#0 "root" testing::parallel
  0ns task#3 "parallel-1" scheduled
  0ns task#2 "parallel-0" Watch::subscribe (×2)
  0ns task#2 "parallel-0" Actor::with
  0ns task#3 "parallel-1" Actor::with
  0ns task#2 "parallel-0" Actor::clone
  0ns task#3 "parallel-1" Watch::subscribe (×2)
  0ns task#3 "parallel-1" Actor::with
  0ns task#2 "parallel-0" Actor::read (×2)
  0ns task#3 "parallel-1" Actor::clone
  0ns task#3 "parallel-1" Actor::read (×2)
  0ns task#3 "parallel-1" Actor::with (×2)
  0ns task#3 "parallel-1" Actor::drop
  0ns task#3 "parallel-1" Watcher::drop
  0ns task#3 "parallel-1" Actor::drop
  0ns task#2 "parallel-0" Actor::with (×2)
  0ns task#2 "parallel-0" Actor::drop
  0ns task#2 "parallel-0" Watcher::drop
  0ns task#2 "parallel-0" Actor::drop
  0ns task#0 "root" scheduled
  0ns task#0 "root" Actor::read (×2)
  0ns task#0 "root" Watch::drop (×2)
  0ns failed (Panic) at task#0 "root"
```

It has as many steps as before, since every operation is still needed, but
each worker now finishes its drop without interruption once both have read.

A `Timeline` is data as well as text. `events()` returns each
`TimelineEvent`, whose `kind()` is a `TimelineEventKind` such as `Scheduled`,
`Wait`, `Wakeup` or `Failed`, and whose `scheduled_operation()` names the
operation. `tasks()` lists a `TaskInfo` for each task. `waiting()` lists a
`WaitingTask` for each task still blocked when the execution ended, with the
operation it waits on. Read `waiting()` first for a `Deadlock`.

`Replay::save` writes the record as JSON, and `Replay::load` reads it back
and reports any incompatible field. `testing::replay` runs one record with
fresh state and must follow its schedule exactly. `testing::replay_corpus`
runs each record in a list of files, in order, and returns the first failure
as a `CorpusError` with its path. Check the files into the test suite, for
example under `tests/schedules/`, and replay them in a test. The record format
is versioned, currently version 5. Regenerate records written by an earlier
version.

A record pins a schedule, not a test. `replay_corpus` must take each
record's scheduling steps in order, and then runs the rest of the scenario
round-robin. So a record keeps working after a fix that changes values but
not operations, such as a wrong condition inside one `with`. The fix for this
bug is the `Drop` in the first example: one `with` that both decrements and
removes. Each drop then takes fewer steps, and `replay_corpus` reports a
`ReplayMismatch` for the record. Delete a record when that happens. `check`
explores the scenario again on every run.

## Test a protocol

The second example is a small key-value protocol over a byte stream. Each
message is a frame: a two-byte big-endian length, then that many bytes. The
server looks up each key in a store and replies `ok <value>`, `missing` or
`unavailable`. The code is written against Tokio's `AsyncRead` and
`AsyncWrite`, so the tests give it a modeled stream in place of a socket, and
a mock in place of the store.

```rust
use std::{collections::BTreeMap, future::Future, io, pin::Pin, time::Duration};

use adamantium::{
    Cancellation, UntilError,
    testing::{
        self, Config, DuplexConfig, FaultInjector, FaultRule, FaultStep, Latency, LinkConfig,
        TransportFault,
    },
    time, until,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Writes `payload` as one frame.
async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> io::Result<()> {
    let len = u16::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too long"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// Reads one frame. `None` means the peer closed the stream between frames.
/// A stream that ends inside a frame is an `UnexpectedEof` error.
async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Vec<u8>>> {
    let mut high = [0];
    if stream.read(&mut high).await? == 0 {
        return Ok(None);
    }
    let low = stream.read_u8().await?;
    let mut payload = vec![0; usize::from(u16::from_be_bytes([high[0], low]))];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreError {
    Unavailable,
}

/// What the server needs from its database.
trait Store: Sync {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<String>, StoreError>> + Send;
}

/// Answers each request until the client closes the connection.
async fn serve(store: &impl Store, mut stream: impl AsyncRead + AsyncWrite + Unpin) -> io::Result<()> {
    while let Some(key) = read_frame(&mut stream).await? {
        let reply = match store.get(&String::from_utf8_lossy(&key)).await {
            Ok(Some(value)) => format!("ok {value}"),
            Ok(None) => "missing".to_owned(),
            Err(StoreError::Unavailable) => "unavailable".to_owned(),
        };
        write_frame(&mut stream, reply.as_bytes()).await?;
    }
    Ok(())
}

/// Sends one request and reads the reply.
async fn get(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), key: &str) -> io::Result<String> {
    write_frame(stream, key.as_bytes()).await?;
    let reply = read_frame(stream).await?.ok_or(io::ErrorKind::UnexpectedEof)?;
    String::from_utf8(reply).map_err(|_| io::ErrorKind::InvalidData.into())
}

/// A store in memory, which `faults` slows down and breaks.
struct MockStore {
    data: BTreeMap<String, String>,
    faults: FaultInjector<StoreError>,
}

impl MockStore {
    fn new(faults: FaultInjector<StoreError>) -> Self {
        let data = BTreeMap::from([("alpha".to_owned(), "1".to_owned())]);
        Self { data, faults }
    }
}

impl Store for MockStore {
    async fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.faults.inject("get").await?;
        Ok(self.data.get(key).cloned())
    }
}

type Peer = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

/// Runs the server and the client side by side. The client must succeed. The
/// server may fail with `BrokenPipe`: the client can hang up while the server
/// is still flushing its last reply.
async fn run(server: Peer, client: Peer) {
    let [served, asked] = testing::parallel([server, client]).await.try_into().unwrap();
    asked.unwrap();
    if let Err(error) = served {
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}

/// Two requests over a link with 20 ms of latency each way, which delivers at
/// most three bytes to each read.
async fn requests_over_a_slow_link() {
    let link = LinkConfig {
        latency: Duration::from_millis(20),
        max_read_chunk: Some(3),
        ..LinkConfig::default()
    };
    let (mut client, server) = testing::duplex_with(DuplexConfig::symmetric(link));
    let store = MockStore::new(FaultInjector::new());
    let start = time::now();
    let server: Peer = Box::pin(async move { serve(&store, server).await });
    let client: Peer = Box::pin(async move {
        assert_eq!(get(&mut client, "alpha").await?, "ok 1");
        assert_eq!(get(&mut client, "beta").await?, "missing");
        assert_eq!(time::now() - start, Duration::from_millis(80));
        Ok(())
    });
    run(server, client).await;
}

/// The connection ends after `cut` bytes of the reply. The client must get an
/// error, never a short value.
async fn reply_cut_short(cut: usize) {
    let to_client = LinkConfig {
        disconnect_after: Some((cut, TransportFault::Eof)),
        ..LinkConfig::default()
    };
    let (mut client, server) =
        testing::duplex_with(DuplexConfig::new(LinkConfig::default(), to_client));
    let store = MockStore::new(FaultInjector::new());
    let server: Peer = Box::pin(async move { serve(&store, server).await });
    let client: Peer = Box::pin(async move {
        let error = get(&mut client, "alpha").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        Ok(())
    });
    run(server, client).await;
}

/// The store takes 50 ms to answer, and the network goes down at 30 ms.
async fn connection_drops_while_waiting() {
    let (mut client, server, control) = testing::duplex_controlled(DuplexConfig::default());
    let slow = FaultInjector::new().with_op_schedule("get", [FaultStep::delay(Duration::from_millis(50))]);
    let store = MockStore::new(slow);
    let start = time::now();
    let peers: [Peer; 3] = [
        Box::pin(async move {
            let error = serve(&store, server).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
            Ok(())
        }),
        Box::pin(async move {
            let error = get(&mut client, "alpha").await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
            assert_eq!(time::now() - start, Duration::from_millis(30));
            Ok(())
        }),
        Box::pin(async move {
            time::sleep(Duration::from_millis(30)).await;
            control.disconnect(TransportFault::Error(io::ErrorKind::ConnectionReset));
            Ok(())
        }),
    ];
    for result in testing::parallel(peers).await {
        result.unwrap();
    }
}

/// The first store call fails. The server reports it and keeps serving.
async fn store_fails_once() {
    let (mut client, server) = testing::duplex(4096);
    let faults = FaultInjector::new().with_op_schedule("get", [FaultStep::fail(StoreError::Unavailable)]);
    let store = MockStore::new(faults.clone());
    let server: Peer = Box::pin(async move { serve(&store, server).await });
    let client: Peer = Box::pin(async move {
        assert_eq!(get(&mut client, "alpha").await?, "unavailable");
        assert_eq!(get(&mut client, "alpha").await?, "ok 1");
        Ok(())
    });
    run(server, client).await;
    assert_eq!(faults.calls("get"), 2);
}

/// The store takes five seconds. The client gives up after 100 ms.
async fn client_deadline() {
    let (mut client, server) = testing::duplex(4096);
    let slow = FaultInjector::new().with_default_rule(FaultRule::delay(Latency::fixed(Duration::from_secs(5))));
    let store = MockStore::new(slow);
    let start = time::now();
    let server: Peer = Box::pin(async move { serve(&store, server).await });
    let client: Peer = Box::pin(async move {
        let cancel = Cancellation::new();
        let deadline = start + Duration::from_millis(100);
        let reply = until(&cancel, Some(deadline), get(&mut client, "alpha")).await;
        assert!(matches!(reply, Err(UntilError::DeadlineExceeded)));
        assert_eq!(time::now(), deadline);
        Ok(())
    });
    let [served, asked] = testing::parallel([server, client]).await.try_into().unwrap();
    asked.unwrap();
    // The client hung up, so the late reply has nowhere to go.
    assert_eq!(served.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
}

testing::check(Config::default(), requests_over_a_slow_link).unwrap();
// The reply "ok 1" is a six-byte frame. Cut it at every byte.
for cut in 0..6 {
    testing::check(Config::default(), move || reply_cut_short(cut)).unwrap();
}
testing::check(Config::default(), connection_drops_while_waiting).unwrap();
testing::check(Config::default(), store_fails_once).unwrap();
testing::check(Config::default(), client_deadline).unwrap();
```

The pieces:

- `testing::duplex(capacity)` returns two connected `DuplexStream` ends. Each
  end implements `AsyncRead` and `AsyncWrite`, and its reads and writes are
  scheduling points. `testing::duplex_with` takes a `DuplexConfig`, and
  `testing::duplex_controlled` also returns a `DuplexControl`.
- A `LinkConfig` describes one direction: the buffer capacity, the largest
  chunk a read or a write moves, the latency, and `disconnect_after`, which
  ends the direction with a `TransportFault` after a number of bytes.
  `DuplexConfig::symmetric` uses one link both ways. `DuplexConfig::new`
  takes the direction from the first end to the second, then the reverse. A
  small `max_read_chunk` makes reads return partial frames, which tests the
  frame code on short reads.
- `DuplexControl::disconnect` fails both directions at once from another
  task. `fault_a_to_b` and `fault_b_to_a` fail one. A `TransportFault` is
  either `Eof` or an I/O error of a given kind.
- A `FaultInjector` makes a mock slow or failing. Call `inject` with the
  operation's name at the top of each mocked method. A `FaultRule` decides by
  probability, as in `FaultRule::fail(0.2, error)`, or by call number, as in
  `FaultRule::fail_when(|call| call % 3 == 0, error)`, and can add a
  `Latency`. A script of `FaultStep`s gives the outcome of each call in order,
  and comes before any rule. Probabilities draw from the seed and delays are
  virtual, so a replay repeats them. `calls` counts the calls to an operation.
- `until` takes a deadline built from `time::now()`. Under the harness the
  deadline is virtual, so the client gives up after exactly 100 ms of virtual
  time and the test takes no real time.

The `run` helper accepts a `BrokenPipe` from the server because the modeled
stream reports a peer that has hung up as soon as it can, and in some
executions the client hangs up while the server is still flushing its last
reply. An exact assertion on the server's result fails in those executions,
and `check` finds one within the default 100. A server has to handle a client
that leaves at any point, so the test checks that it does.

## Test expiry against the wall clock

Durations and deadlines come from `time::now()`. Timestamps that come from
outside the process, such as a JWT's `exp` or a cookie's expiry, are absolute,
so code compares them with `time::wall_now()`. Outside the harness,
`wall_now` reads the system clock. Inside an execution it starts at
`Config::wall_epoch` and advances with virtual time.

```rust
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use adamantium::{
    Actor,
    testing::{self, Config},
    time,
};

/// Sessions by token, each valid until an absolute expiry.
#[derive(Clone, Default)]
struct Sessions {
    expiry: Actor<BTreeMap<String, SystemTime>>,
}

impl Sessions {
    fn insert(&self, token: &str, exp: SystemTime) {
        self.expiry.with(|expiry| expiry.insert(token.to_owned(), exp));
    }

    /// Whether `token` is still valid. Forgets it once it has expired.
    fn is_valid(&self, token: &str) -> bool {
        let now = time::wall_now();
        self.expiry.with(|expiry| match expiry.get(token) {
            Some(&exp) if now < exp => true,
            Some(_) => {
                expiry.remove(token);
                false
            }
            None => false,
        })
    }
}

/// A token issued at 2027-01-15T08:00:00Z, with `exp` an hour later.
const ISSUED: u64 = 1_800_000_000;
const EXP: u64 = ISSUED + 60 * 60;

let config = Config {
    wall_epoch: UNIX_EPOCH + Duration::from_secs(ISSUED),
    ..Config::default()
};
let record = testing::check(config, || async {
    let sessions = Sessions::default();
    sessions.insert("token", UNIX_EPOCH + Duration::from_secs(EXP));
    let checks = [59, 60].map(|minutes| {
        let sessions = sessions.clone();
        async move {
            time::sleep(Duration::from_secs(minutes * 60)).await;
            sessions.is_valid("token")
        }
    });
    assert_eq!(testing::parallel(checks).await, [true, false]);
})
.unwrap();
assert_eq!(record.wall_epoch(), config.wall_epoch);
```

The token's `exp` is a fixed timestamp, so the test sets `wall_epoch` to the
time the token was issued. Without it, every execution starts at the default,
2026-01-01T00:00:00Z. The record keeps the epoch, and a replay reuses it, so
the replay sees the same timestamps. Timers that fire at the same virtual
instant run in an order the harness chooses, so a refresh and a check that
both land on the expiry are explored in both orders.

## Run the tests

Build the `Config` with `Config::from_env`, so a failure from CI can be rerun
on your machine:

```rust
use adamantium::{
    Actor,
    testing::{self, Config, SchedulerMode},
};

async fn two_increments() {
    let count = Actor::new(0);
    let workers = (0..2).map(|_| {
        let count = count.clone();
        async move { count.with(|n| *n += 1) }
    });
    testing::parallel(workers).await;
    assert_eq!(count.read(|n| *n), 2);
}

let config = Config {
    max_steps: 50_000,
    ..Config::from_env().unwrap()
};
testing::check(config.with_scheduler(SchedulerMode::Pct { depth: 3 }), two_increments).unwrap();
```

`from_env` starts from the defaults and reads two variables:
`ADAMANTIUM_SEED` sets the seed and `ADAMANTIUM_ITERATIONS` the number of
executions. It returns an error that names the variable if a value is
malformed. Set any other fields after it, as above.

To rerun a failure, copy the line the report printed:

```sh
ADAMANTIUM_SEED=0 ADAMANTIUM_ITERATIONS=100 cargo test --test hub two_watchers_leave
```

The rerun repeats the same exploration, so it fails at the same iteration with
the same record, as long as the code and the rest of the `Config` are
unchanged. To keep a schedule across code changes, save its record instead.

A fixed seed explores the same schedules on every run. To cover new ones,
give CI a different seed each run, such as `ADAMANTIUM_SEED=$GITHUB_RUN_NUMBER`.
A failure still prints the seed that found it.

For a long soak run, raise the iteration count and build in release mode. A
test marked `#[ignore]` keeps it out of the normal suite:

```sh
ADAMANTIUM_ITERATIONS=1000000 cargo test --release --test hub -- --ignored
```

## What stays outside the harness

The harness runs the code, so the code has to stay inside what it models.
Three kinds of code don't.

### Code that needs Tokio's reactor

Tokio's sockets, files, processes, signals and timers need a Tokio runtime.
Under the harness there is none, and the first such call panics with "there is no reactor running, must be called from the context of
a Tokio 1.x runtime":

```rust
use adamantium::testing::{self, Config, FailureKind};

let failure = testing::check(Config::default(), || async {
    let _listener = tokio::net::TcpListener::bind("127.0.0.1:0").await;
})
.unwrap_err();
assert_eq!(failure.kind(), FailureKind::Panic);
```

Put the seam where the I/O starts. Write the protocol against `AsyncRead` and
`AsyncWrite`, as `serve` above is, and pass the tests a `DuplexStream`. The
accept loop shrinks to a few lines that only production runs:

```rust
use std::io;

use adamantium::{
    Tasks,
    testing::{self, Config},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
};

/// Echoes every byte back until the client closes its side.
async fn echo(stream: impl AsyncRead + AsyncWrite) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    tokio::io::copy(&mut reader, &mut writer).await?;
    writer.shutdown().await
}

/// Production: accepts connections and serves each one in its own task.
async fn accept_loop(listener: TcpListener) -> io::Result<()> {
    Tasks::run(async |tasks| loop {
        let (stream, _) = listener.accept().await?;
        tasks.spawn(echo(stream));
    })
    .await
}

// Tests: the same `echo`, over a modeled stream.
testing::check(Config::default(), || async {
    let (mut client, server) = testing::duplex(64);
    Tasks::run(async |tasks| {
        tasks.spawn(echo(server));
        client.write_all(b"ping").await?;
        client.shutdown().await?;
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await?;
        assert_eq!(reply, b"ping");
        Ok(())
    })
    .await
    .unwrap();
})
.unwrap();
```

### Tokio's synchronization primitives

`tokio::sync` needs no runtime, so its types run under the harness. But the
harness has no scheduling point inside them. It never switches tasks inside a
`tokio::sync::Mutex` lock or between a `Notify` and the wakeup, and it can't
name them in a timeline or a deadlock report. Use the modeled types for state
the tests should explore: `Actor` for a mutex, `Channel` for `mpsc`, `Watch`
for `watch`, `Reply` for `oneshot`, `Topic` or `Fanout` for `broadcast`, and
`Once` for `OnceCell`.
The [migration guide](migrating.md) shows each rewrite, and
`cargo adamantium check` reports Tokio's locks and its `oneshot`, `watch`,
`broadcast` and unbounded `mpsc` channels.

### A framework that owns the accept loop

A web framework accepts connections and calls your handlers with its own
request and socket types, so it stays outside the harness. Keep each handler
thin: it reads what it needs from the request and calls a function that takes
plain values and Adamantium types. Test that function. For example, a
WebSocket route can check access and hand the socket to a loop that forwards
hub events. The hub runs under the harness with no server. To test the loop
too, give it a small trait for sending and receiving messages in place of the
framework's socket type.

## Choose a scheduler

Random scheduling is the default, and the right choice for most tests. Pass a
`Config` to `check` and it samples `iterations` schedules from `seed`. PCT
(probabilistic concurrency testing) samples task priorities instead and
changes them at a few random points, which finds bugs that need a task to be
preempted at one particular step. `Config::with_scheduler` returns an
`Exploration` that picks the scheduler:

```rust
use adamantium::{
    Actor,
    testing::{self, Config, DfsStatus, FailureKind, SchedulerMode},
};

/// `workers` tasks each add one to a shared count.
async fn increments(workers: usize) {
    let count = Actor::new(0);
    let tasks = (0..workers).map(|_| {
        let count = count.clone();
        async move { count.with(|n| *n += 1) }
    });
    testing::parallel(tasks).await;
    assert_eq!(count.read(|n| *n), workers);
}

// Sampling suits a scenario of any size.
testing::check(Config::default(), || increments(4)).unwrap();
let pct = Config::default().with_scheduler(SchedulerMode::Pct { depth: 3 });
testing::check(pct, || increments(4)).unwrap();

// DFS runs every schedule, and suits a scenario of a handful of steps.
let dfs = Config { iterations: 10_000, ..Config::default() }.with_scheduler(SchedulerMode::Dfs);
let record = testing::check(dfs, || increments(2)).unwrap();
assert_eq!(record.dfs_status(), Some(DfsStatus::Complete));
println!("{} schedules of {} steps", record.iteration(), record.steps());

// With more steps, DFS fails rather than pass after a fraction of them.
let dfs = Config::default().with_scheduler(SchedulerMode::Dfs);
let failure = testing::check(dfs, || increments(4)).unwrap_err();
assert_eq!(failure.kind(), FailureKind::IterationLimit);

// DfsPrefix runs the first `iterations` schedules in DFS order and passes.
let prefix = Config::default().with_scheduler(SchedulerMode::DfsPrefix);
let record = testing::check(prefix, || increments(4)).unwrap();
assert_eq!(record.dfs_status(), Some(DfsStatus::IterationLimit));
```

Depth-first search (DFS) enumerates every schedule, and their number grows
exponentially with the steps in an execution. `record.steps()` gives the
number of steps in a recorded execution. Two workers that each add one take
19 steps, and DFS runs 696 schedules to cover them. Four workers take 36
steps, and DFS has schedules left after the default 100 executions. Each
scenario in the hub example takes dozens. So a `SchedulerMode::Dfs` check that still has schedules left after
`iterations` executions fails with `FailureKind::IterationLimit`, rather than
pass after a fraction of them:

```text
adamantium::testing IterationLimit: DFS ran 100 executions and schedules remain. The last execution took 36 scheduling steps.
DFS suits scenarios of a handful of steps. For this scenario, use SchedulerMode::Pct. To finish the search, raise Config::iterations. To stop after 100 executions without failing, use SchedulerMode::DfsPrefix.
```

That failure has no failing schedule: `failure.replay()` holds the last
execution, which passed. To run only the first `iterations` schedules in DFS
order, ask for `SchedulerMode::DfsPrefix`.

| Mode | What it explores | How exploration ends when executions pass |
| --- | --- | --- |
| `Random` | Samples scheduling choices among runnable tasks using `seed`. | Runs `iterations` executions. |
| `Pct { depth }` | Samples task priorities using `seed`. After calibration, each execution has at most `depth - 1` sampled priority changes. Explicit yields can also lower priorities. | Runs `iterations` executions, including calibration. Stops after calibration if that execution offered no task choices. |
| `Dfs` | Enumerates modeled task choices. Each execution starts with the same fixed random data stream. | Passes after every reachable schedule runs, with `record.dfs_status()` giving `Some(DfsStatus::Complete)`. Fails with `FailureKind::IterationLimit` if schedules remain after `iterations` executions. |
| `DfsPrefix` | Enumerates task choices in the same order as `Dfs`. | Passes after every reachable schedule runs or after `iterations` executions, whichever comes first. `record.dfs_status()` gives `Some(DfsStatus::Complete)` or `Some(DfsStatus::IterationLimit)`. |

`iterations` limits executions. `max_steps` limits the scheduling steps in
each execution, and running out fails with `FailureKind::StepLimit`. Both must
be positive, as must the PCT `depth`. DFS covers the task choices for the
fixture you supply and its fixed random data, so vary the fixture to cover
other inputs.

## Reference

### What the harness schedules

`Actor`, `Channel`, `Fanout`, `Reply`, `Watch`, `SingleFlight`, `Once`,
`Tasks`, and `Cancellation` support deterministic scheduling. So do the
`testing::DuplexStream` ends and `testing::FaultInjector`.

- The harness schedules synchronous `Actor` calls and ownership operations,
  such as clones and drops. A `read` followed by a separate `with` can show a
  lost update with no `await` between them.
- A panic, a deadlock, or running out of the step limit ends exploration with
  a failure.

Channels and watches:

- The harness covers synchronous channel operations, competing senders and receivers, streams, close-and-drain, endpoint drops, and cancellation by drop of a pending future.
- `Channel`, `Fanout`, and `Watch` share a private notification boundary that registers and then checks again.
- The bounded storage of `Channel` is a concurrent ring in production and a locked FIFO in the harness. The notification, closure, and endpoint ownership logic is shared.
- Async channel operations and limit acquisitions share a budget within each poll of the test future, a `testing::parallel` worker, or a scoped task. Exhaustion yields before changing channel state or taking a place. Replay includes that scheduling point.
- The harness schedules registration, notification, polling, and cancellation handoff. It treats each event-listener operation as one atomic notification step. It does not model the internal atomics of event-listener.

`SingleFlight`:

- The harness schedules joining, leaving, the hand-off of the operation between callers, and the end of a flight. A call can arrive while the last caller of an earlier operation is still dropping it.
- The operation's waker never switches tasks. It marks the operation woken and notifies the callers, which take turns polling it.

`Once`:

- The harness schedules reading the cell, taking the claim to initialize it,
  waiting, filling it, and the hand-off to a waiting call after an
  initializer fails, panics, or is dropped.
- The initializer runs inside its call, so the harness schedules it as part
  of the task that awaits the call.

Tasks:

- `Tasks` keeps its rules for result claims, the oldest unclaimed error, panic precedence, and abort-and-drain cleanup. This includes concurrent maps and the ordered and unordered streams.
- Blocking work models the boundary between queued and started. A queued closure can be aborted. A started closure finishes before a scope joins.
- Replace real blocking I/O with deterministic fixtures that use the modeled primitives.

Time:

- Operations that take a cancellation accept virtual deadlines built from `time::now()`.
- Dropping a sleep removes its timer.
- A sleep belongs to the execution in which it is first polled. The harness rejects a pending sleep that is carried into another execution.
- The clock origin is opaque and shared within a process. A replay in another process keeps the elapsed virtual durations.
- An `Instant` carries no execution identity. Build deadlines inside each execution of the factory.
- `time::wall_now()` starts every execution at `Config::wall_epoch`, 2026-01-01T00:00:00Z by default, and advances in lockstep with `time::now()`. A `Replay` records the epoch and a replay reuses it, so `exp` checks against absolute timestamps repeat exactly.

Randomness:

- `adamantium::random` draws from the scheduler. The seed decides the draws, and a `Replay` records each one and repeats it.

### Rules for tests

- Use `#[test]`, not `#[tokio::test]`.
- Create primitives inside the execution that uses them.
- Pass values into the factory, not through thread-locals. Each harness call runs on a new thread named like the test's. It keeps the test's `tracing` subscriber and current span.
- Draw random numbers from `adamantium::random`. Read time from `time::now()` and `time::wall_now()`. Replace I/O, other clocks, other randomness, and foreign executors with deterministic fixtures.

### Limits

- OS signal handling is outside the harness.
- The harness cannot diagnose every foreign future.
- A step bound cannot interrupt a loop that makes no scheduling calls.
