# Migrating to Adamantium

Most of what `cargo adamantium check` reports in an existing async crate is
shared state behind `Arc<Mutex<T>>` and tasks started with `tokio::spawn`.
This guide shows the rewrite for each common form of the two. Each section has
the code as it was and the code as the rules want it. Every block runs as a
doctest of the library, so the rewrites track the current API.

To adopt the rules one module at a time, skip the paths you haven't migrated
yet with the [skip list](cli.md#skip-list).

A rewrite is mechanical when it needs no decision about what the program
should do. "Mostly" means it is mechanical in the common case shown, but
changes behavior in a way to check.

| Before | After | Mechanical |
| --- | --- | --- |
| `Arc::new(Mutex::new(x))`, `Arc<Mutex<T>>` | `Actor::new(x)`, `Actor<T>` | Yes |
| A guard used within one statement | `with` or `read` | Yes |
| `lock().unwrap().clone()` | `snapshot()` | Yes |
| `Arc::try_unwrap(m)` then `into_inner()` | `into_inner()` | Yes |
| A guard held across statements | A method on the state | No |
| `RwLock<T>` | `Actor<T>`, often holding a copy-on-write value | No |
| A lock held across an await | Read, await, then record | No |
| Two locks held at once | One actor, or one after the other | No |
| `tokio::task::spawn_blocking(f).await.unwrap()` | `blocking(f).await` | Yes |
| `std::thread::spawn` | `blocking` | Mostly |
| `tokio::spawn` with the handle awaited in the same function | `Tasks::run` | Mostly |
| A loop of spawns, or results pushed into an `Arc<Mutex<Vec<T>>>` | `concurrently` | Mostly |
| `JoinSet::join_next` | `concurrently_unordered` | No |
| `select!` racing futures for the first success | `first_ok` | Mostly |
| `select!` waiting for the first of several events | `race` | Mostly |
| A detached worker fed by a channel | A worker task in the scope that sends to it | No |
| `JoinHandle::abort` on a long-lived task | `Tasks::run_until` with a `Cancellation` | No |
| `tokio::spawn` inside a task | A scope inside the task | No |
| `tokio::time::sleep` | `time::sleep` | Yes |
| `tokio::time::interval` | A loop over `time::sleep_until` | Mostly |

## Shared state

`Actor<T>` is an `Arc<Mutex<T>>` whose lock is taken only by a synchronous
closure. Cloning an actor shares its state, as cloning the `Arc` did. The
closure can't await, so the lock can't be held across an await, and calling
any actor from inside it panics, so two locks are never held at once. A panic
inside a closure doesn't poison the actor: the next caller sees the state as
the closure left it, so the `unwrap` on every `lock()` goes away.

`fix` can migrate a bounded async chain when it proves where every future
is polled: private functions/methods called only with direct `.await`, and
immediately awaited blocks, all the way to a verified Tokio binary entry.
Immediate await alone is not enough; an enclosing future could be stored and
later polled inside an actor callback. Stored, returned, boxed, externally
passed or public-library futures remain manual, as do custom executors and
macro wrappers. One unproved path keeps the entire connected state unchanged.
The CLI reports that boundary. See the [CLI coverage](../README.md)
for the deliberately narrow entry-point rules.

### Lock, change, unlock

Before:

```rust
use std::sync::{Arc, Mutex};

let hits = Arc::new(Mutex::new(0u64));
let handle = Arc::clone(&hits);
*handle.lock().unwrap() += 1;
assert_eq!(*hits.lock().unwrap(), 1);
```

After:

```rust
use adamantium::Actor;

let hits = Actor::new(0u64);
let handle = hits.clone();
handle.with(|hits| *hits += 1);
assert_eq!(hits.read(|hits| *hits), 1);
```

`Arc::new(Mutex::new(x))` becomes `Actor::new(x)`, a field of type
`Arc<Mutex<T>>` becomes `Actor<T>`, and `Arc::clone(&m)` becomes `m.clone()`.
A guard that lives for one statement becomes the body of a `with` closure, or
of a `read` closure if it only reads.

### A guard held across statements

A guard held for several statements becomes a method on the state. The method
is plain Rust, so it can be tested with no tasks at all, and a read-only method
can be passed to `read` by name.

Before:

```rust
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Cart {
    items: Vec<String>,
    total: u64,
}

let cart = Arc::new(Mutex::new(Cart::default()));
{
    let mut cart = cart.lock().unwrap();
    if !cart.items.contains(&"pen".to_owned()) {
        cart.items.push("pen".to_owned());
        cart.total += 3;
    }
}
assert_eq!(cart.lock().unwrap().total, 3);
```

After:

```rust
use adamantium::Actor;

#[derive(Default)]
struct Cart {
    items: Vec<String>,
    total: u64,
}

impl Cart {
    fn add(&mut self, item: &str, price: u64) -> bool {
        if self.items.iter().any(|i| i == item) {
            return false;
        }
        self.items.push(item.to_owned());
        self.total += price;
        true
    }

    fn total(&self) -> u64 {
        self.total
    }
}

let cart = Actor::new(Cart::default());
assert!(cart.with(|c| c.add("pen", 3)));
assert!(!cart.with(|c| c.add("pen", 3)));
assert_eq!(cart.read(Cart::total), 3);
```

An early `return` or `break` inside the guarded block becomes the method's
return value, which `with` passes back.

### Copying the state out

Before:

```rust
use std::sync::{Arc, Mutex};

let settings = Arc::new(Mutex::new(vec!["verbose".to_owned()]));
let current = settings.lock().unwrap().clone();
assert_eq!(current, ["verbose"]);
```

After:

```rust
use adamantium::Actor;

let settings = Actor::new(vec!["verbose".to_owned()]);
let current = settings.snapshot();
assert_eq!(current, ["verbose"]);
```

### Taking the state back

Before:

```rust
use std::sync::{Arc, Mutex};

let log = Arc::new(Mutex::new(vec![1, 2]));
let log = Arc::try_unwrap(log).unwrap().into_inner().unwrap();
assert_eq!(log, [1, 2]);
```

After:

```rust
use adamantium::Actor;

let log = Actor::new(vec![1, 2]);
assert_eq!(log.into_inner().ok(), Some(vec![1, 2]));
```

While another handle shares the state, `into_inner` returns the actor back as
its error, as `Arc::try_unwrap` returned the `Arc`.

### `RwLock`

`Actor::read` takes the same lock as `with`, so readers wait for each other.
For state that many tasks read and few write, keep a copy-on-write value in
the actor and give readers a snapshot. A snapshot of a `Map`, `Array` or `Str`
shares storage with the actor's copy, so taking one is cheap, and readers then
use it with no lock at all. The next write after a snapshot copies the value
once.

Before:

```rust
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

let routes = Arc::new(RwLock::new(HashMap::from([("/", "home")])));
assert_eq!(routes.read().unwrap().get("/"), Some(&"home"));
routes.write().unwrap().insert("/about", "about");
assert_eq!(routes.read().unwrap().len(), 2);
```

After:

```rust
use adamantium::{Actor, Map};

let routes = Actor::new(Map::from([("/", "home")]));
let view = routes.snapshot();
assert_eq!(view.get("/"), Some(&"home"));
routes.with(|r| r.insert("/about", "about"));
assert_eq!(view.len(), 1);
assert_eq!(routes.read(Map::len), 2);
```

### A lock held across an await

An async mutex lets one task hold the state while it waits on the network.
With an actor, the task reads what it needs, awaits with no lock held, and
then records the result.

Before:

```rust
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

async fn user(
    cache: &Mutex<HashMap<u32, String>>,
    id: u32,
) -> Result<String, std::io::Error> {
    let mut cache = cache.lock().await;
    if let Some(user) = cache.get(&id) {
        return Ok(user.clone());
    }
    let user = fetch(id).await?;
    cache.insert(id, user.clone());
    Ok(user)
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let cache = Arc::new(Mutex::new(HashMap::new()));
    assert_eq!(user(&cache, 7).await?, "user 7");
    assert_eq!(user(&cache, 7).await?, "user 7");
    Ok(())
}
```

After:

```rust
use adamantium::{Actor, Map};

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

async fn user(
    cache: &Actor<Map<u32, String>>,
    id: u32,
) -> Result<String, std::io::Error> {
    if let Some(user) = cache.read(|c| c.get(&id).cloned()) {
        return Ok(user);
    }
    let user = fetch(id).await?;
    Ok(cache.with(|c| c.entry(id).or_insert(user).clone()))
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let cache = Actor::new(Map::new());
    assert_eq!(user(&cache, 7).await?, "user 7");
    assert_eq!(user(&cache, 7).await?, "user 7");
    Ok(())
}
```

This isn't mechanical, because the state can change while the task awaits.
Here two tasks that miss the cache at once both fetch, and `or_insert` keeps
whichever result is recorded first. Decide what each such race should do.
Where the work must happen only once, give it to one task that receives
requests on a channel, as in [a detached worker](#a-detached-worker).

### Two locks at once

Holding one lock while taking another deadlocks when two tasks take them in
opposite orders. Calling an actor from inside another actor's closure panics
instead. If an invariant spans both values, put them in one actor. Otherwise,
finish with the first actor before calling the second.

Before:

```rust
use std::sync::{Arc, Mutex};

let checking = Arc::new(Mutex::new(100u64));
let savings = Arc::new(Mutex::new(0u64));
{
    let mut from = checking.lock().unwrap();
    let mut to = savings.lock().unwrap();
    *from -= 30;
    *to += 30;
}
assert_eq!(*checking.lock().unwrap() + *savings.lock().unwrap(), 100);
```

After:

```rust
use adamantium::Actor;

struct Accounts {
    checking: u64,
    savings: u64,
}

impl Accounts {
    fn transfer(&mut self, amount: u64) -> bool {
        let Some(rest) = self.checking.checked_sub(amount) else {
            return false;
        };
        self.checking = rest;
        self.savings += amount;
        true
    }
}

let accounts = Actor::new(Accounts { checking: 100, savings: 0 });
assert!(accounts.with(|a| a.transfer(30)));
assert_eq!(accounts.read(|a| a.checking + a.savings), 100);
```

A nested call panics, whichever actor it reaches:

```rust,should_panic
use adamantium::Actor;

let checking = Actor::new(100u64);
let savings = Actor::new(0u64);
checking.with(|from| {
    *from -= 30;
    savings.with(|to| *to += 30); // panics: an actor method can't call an actor
});
```

## Tasks

A task spawned in a `Tasks` scope can't outlive the scope: when the scope's
future finishes, every task in it has finished too. A task returns
`Result<T, E>`, and a panic in a task resumes in the scope. The `JoinError`
that a Tokio `JoinHandle` returns, and the `unwrap` that usually follows it,
go away.

A task's error ends the scope, aborting the other tasks, once the scope sees
it: when `value` returns it to the body and the body passes it on, or when the
body has returned and the scope joins. Until then a failed task doesn't
interrupt the body. A body waiting on `value` for a task that never finishes
keeps waiting after a sibling fails, so bound such waits with `run_until`.

Dropping a scope's future before it finishes, such as by aborting the task that
awaits it, requests aborts of its tasks but can't wait for them.

### Spawning and awaiting the handle

Before:

```rust
async fn fetch(path: &str) -> Result<String, std::io::Error> {
    Ok(path.to_owned())
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let profile = tokio::spawn(fetch("/profile"));
    let feed = tokio::spawn(fetch("/feed"));
    let profile = profile.await.unwrap()?;
    let feed = feed.await.unwrap()?;
    assert_eq!((profile.as_str(), feed.as_str()), ("/profile", "/feed"));
    Ok(())
}
```

After:

```rust
use adamantium::Tasks;

async fn fetch(path: &str) -> Result<String, std::io::Error> {
    Ok(path.to_owned())
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let (profile, feed) = Tasks::run(async |t| {
        let profile = t.spawn(fetch("/profile"));
        let feed = t.spawn(fetch("/feed"));
        Ok((t.value(profile).await?, t.value(feed).await?))
    })
    .await?;
    assert_eq!((profile.as_str(), feed.as_str()), ("/profile", "/feed"));
    Ok(())
}
```

Wrap the code that spawns and awaits in `Tasks::run`, change `tokio::spawn(f)`
to `t.spawn(f)`, and change `handle.await` to `t.value(handle).await`. One
behavior changes: when `profile` fails, `feed` is aborted, where before it ran
on detached. A failure of `feed` is seen only once `profile` finishes and the
body asks for `feed`, as before. If a handle was returned or stored instead of
awaited, the rewrite has to move the scope up to where the task should end.

A task's future must return a `Result`. For work that can't fail, name the
error type `Infallible` and destructure the result:

```rust
use adamantium::Tasks;
use std::convert::Infallible;

#[tokio::main]
async fn main() {
    let Ok(sum) = Tasks::<Infallible>::run(async |t| {
        let left = t.spawn(async { Ok((1..=50).sum::<u32>()) });
        let right = t.spawn(async { Ok((51..=100).sum::<u32>()) });
        Ok(t.value(left).await? + t.value(right).await?)
    })
    .await;
    assert_eq!(sum, 5050);
}
```

### A loop of spawns

Spawning in a loop and awaiting each handle, or having each task push into a
shared `Vec`, becomes `concurrently`. It returns the results in input order,
runs at most `limit` items at a time, and stops at the first error.

Before:

```rust
use std::sync::{Arc, Mutex};

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let users = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for id in 0..20 {
        let users = Arc::clone(&users);
        handles.push(tokio::spawn(async move {
            let user = fetch(id).await?;
            users.lock().unwrap().push(user);
            Ok::<_, std::io::Error>(())
        }));
    }
    for handle in handles {
        handle.await.unwrap()?;
    }
    assert_eq!(users.lock().unwrap().len(), 20);
    Ok(())
}
```

After:

```rust
use adamantium::concurrently;

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let users = concurrently(0..20, 8, fetch).await?;
    assert_eq!(users.len(), 20);
    assert_eq!(users.first().map(String::as_str), Some("user 0"));
    Ok(())
}
```

The old loop started every item at once. Choose a limit; passing the number of
items keeps the old behavior. To keep the loop's shape instead, spawn into a
scope and collect the handles:

```rust
use adamantium::Tasks;

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let users = Tasks::run(async |t| {
        let handles: Vec<_> = (0..20).map(|id| t.spawn(fetch(id))).collect();
        let mut users = Vec::new();
        for handle in handles {
            users.push(t.value(handle).await?);
        }
        Ok(users)
    })
    .await?;
    assert_eq!(users.len(), 20);
    Ok(())
}
```

A scope waits for its tasks even when nobody asks for their values, and
reports an error from any of them, so fire-and-forget spawns that must finish
before the function returns need no handles at all:

```rust
use adamantium::{Actor, Tasks};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let done = Actor::new(0u32);
    Tasks::<std::io::Error>::run(async |t| {
        for _ in 0..5 {
            let done = done.clone();
            t.spawn(async move {
                done.with(|d| *d += 1);
                Ok(())
            });
        }
        Ok(())
    })
    .await?;
    assert_eq!(done.read(|d| *d), 5);
    Ok(())
}
```

### Results as they finish

`JoinSet::join_next` hands back results in the order tasks finish. The
streaming form of `concurrently` does the same within a limit.

Before:

```rust
use tokio::task::JoinSet;

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let mut set = JoinSet::new();
    for id in 0..20 {
        set.spawn(fetch(id));
    }
    let mut seen = 0;
    while let Some(user) = set.join_next().await {
        user.unwrap()?;
        seen += 1;
    }
    assert_eq!(seen, 20);
    Ok(())
}
```

After:

```rust
use adamantium::{ScopeError, concurrently_unordered};
use futures_util::StreamExt;

async fn fetch(id: u32) -> Result<String, std::io::Error> {
    Ok(format!("user {id}"))
}

#[tokio::main]
async fn main() -> Result<(), ScopeError<std::io::Error>> {
    let users = concurrently_unordered(0..20, 8, fetch);
    let mut users = std::pin::pin!(users);
    let mut seen = 0;
    while let Some(user) = users.next().await {
        user?;
        seen += 1;
    }
    assert_eq!(seen, 20);
    Ok(())
}
```

Use `concurrently_ordered` for input order. `StreamExt` comes from
`futures-util`, which the project adds itself.

### The first to succeed

Racing futures with `select!` and keeping the first success means looping
until a branch succeeds, disabling the branches that failed, and remembering
that the losers are cancelled only by being dropped, which waits for none of
them. `first_ok` takes items the way `concurrently` does, keeps going past
failures, and aborts and drains the rest once one succeeds. If every item
fails, it returns all the errors in input order.

Before:

```rust
async fn fetch(mirror: &str) -> Result<String, String> {
    if mirror == "a.example.com" {
        return Err(format!("{mirror} is down"));
    }
    Ok(format!("page from {mirror}"))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let a = fetch("a.example.com");
    let b = fetch("b.example.com");
    tokio::pin!(a, b);
    let (mut a_done, mut b_done) = (false, false);
    let page = loop {
        tokio::select! {
            result = &mut a, if !a_done => match result {
                Ok(page) => break page,
                Err(error) if b_done => return Err(error),
                Err(_) => a_done = true,
            },
            result = &mut b, if !b_done => match result {
                Ok(page) => break page,
                Err(error) if a_done => return Err(error),
                Err(_) => b_done = true,
            },
        }
    };
    assert_eq!(page, "page from b.example.com");
    Ok(())
}
```

After:

```rust
use adamantium::first_ok;

async fn fetch(mirror: &str) -> Result<String, String> {
    if mirror == "a.example.com" {
        return Err(format!("{mirror} is down"));
    }
    Ok(format!("page from {mirror}"))
}

#[tokio::main]
async fn main() -> Result<(), Vec<String>> {
    let mirrors = ["a.example.com", "b.example.com"];
    let page = first_ok(mirrors, mirrors.len(), fetch).await?;
    assert_eq!(page, "page from b.example.com");
    Ok(())
}
```

Each item runs as a task, so its future must be `'static`. A limit of one
tries the items in turn, which replaces a chain of fallbacks.

### The first event

A `select!` that waits for a message, a timer, or a shutdown request, and
acts on whichever comes first, becomes `race`. `race` is a function, so the
branches are ordinary futures and the result is an ordinary value. It polls
the futures in the order written, where `select!` picks a branch at random,
so put the event that must win a tie first.

Before:

```rust
use adamantium::Channel;

enum Event {
    Job(u32),
    Stop,
}

#[tokio::main]
async fn main() {
    let jobs = Channel::<u32>::bounded(4);
    let stop = Channel::<()>::bounded(1);
    jobs.send(7).await.ok();

    let event = tokio::select! {
        _ = stop.receive() => Event::Stop,
        job = jobs.receive() => job.map_or(Event::Stop, Event::Job),
    };
    assert!(matches!(event, Event::Job(7)));
}
```

After:

```rust
use adamantium::{Channel, race};

enum Event {
    Job(u32),
    Stop,
}

#[tokio::main]
async fn main() {
    let jobs = Channel::<u32>::bounded(4);
    let stop = Channel::<()>::bounded(1);
    jobs.send(7).await.ok();

    let event = race((
        async { stop.receive().await.map_or(Event::Stop, |()| Event::Stop) },
        async { jobs.receive().await.map_or(Event::Stop, Event::Job) },
    ))
    .await;
    assert!(matches!(event, Event::Job(7)));
}
```

As with `select!`, a future that loses is dropped at the await where it was
waiting. Race only futures that lose nothing by that, such as a receive or a
sleep. To stop on a `Cancellation` or a deadline, prefer `receive_until` and
`send_until`, which need no race.

### Blocking work

`tokio::task::spawn_blocking` and `std::thread::spawn` become `blocking`,
which resumes a panic in the closure on the calling task instead of returning
a `JoinError`.

Before:

```rust
#[tokio::main]
async fn main() {
    let text = "a long document".to_owned();
    let words = tokio::task::spawn_blocking(move || text.split(' ').count())
        .await
        .unwrap();
    assert_eq!(words, 3);
}
```

After:

```rust
use adamantium::blocking;

#[tokio::main]
async fn main() {
    let text = "a long document".to_owned();
    let words = blocking(move || text.split(' ').count()).await;
    assert_eq!(words, 3);
}
```

Dropping the future that `blocking` returns leaves a started closure running.
When a scope must wait for the closure to finish, register it with
`Tasks::spawn_blocking` instead, as the `run_until` example in the API docs
shows.

### A detached worker

A worker spawned on its own, fed by a channel, becomes a task in the scope
that sends to it. Dropping the last sender closes the channel; the worker
drains what's queued, ends, and the scope joins it. If the worker fails, the
next send returns `Closed`, and the scope returns the worker's error.

Before:

```rust
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let saved = Arc::new(Mutex::new(Vec::new()));
    let (sender, mut receiver) = mpsc::channel(16);
    let worker = {
        let saved = Arc::clone(&saved);
        tokio::spawn(async move {
            while let Some(line) = receiver.recv().await {
                saved.lock().unwrap().push(line);
            }
        })
    };
    for line in ["a", "b", "c"] {
        sender.send(line).await.unwrap();
    }
    drop(sender);
    worker.await.unwrap();
    assert_eq!(*saved.lock().unwrap(), ["a", "b", "c"]);
}
```

After:

```rust
use adamantium::{Actor, Channel, Tasks};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let saved = Actor::new(Vec::new());
    let worker_saved = saved.clone();
    let (sender, receiver) = Channel::bounded(16).split();
    Tasks::<std::io::Error>::run(async move |t| {
        t.spawn(async move {
            while let Some(line) = receiver.receive().await {
                worker_saved.with(|s| s.push(line));
            }
            Ok(())
        });
        for line in ["a", "b", "c"] {
            if sender.send(line).await.is_err() {
                break; // The worker stopped; the scope returns its error.
            }
        }
        drop(sender);
        Ok(())
    })
    .await?;
    assert_eq!(saved.snapshot(), ["a", "b", "c"]);
    Ok(())
}
```

### Stopping a long-lived task

A task that runs until it's aborted, such as a timer, becomes a task in a
scope run with `Tasks::run_until`. Cancelling the scope's `Cancellation`
aborts the task and waits for it to stop. The scope then returns
`ScopeError::Cancelled`, which here is the expected way to end. In a server,
a signal handler would call `cancel`.

Before:

```rust
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[tokio::main]
async fn main() {
    let flushes = Arc::new(Mutex::new(0u32));
    let flusher = {
        let flushes = Arc::clone(&flushes);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(5));
            loop {
                ticks.tick().await;
                *flushes.lock().unwrap() += 1;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    flusher.abort();
    assert!(*flushes.lock().unwrap() > 0);
}
```

After:

```rust
use adamantium::{Actor, Cancellation, ScopeError, Tasks, time};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), ScopeError<std::io::Error>> {
    let flushes = Actor::new(0u32);
    let shutdown = Cancellation::new();
    let result = Tasks::run_until(shutdown.clone(), None, async |t| {
        let flushes = flushes.clone();
        t.spawn::<(), _>(async move {
            let period = Duration::from_millis(5);
            let mut next = time::now() + period;
            loop {
                time::sleep_until(next).await;
                next += period;
                flushes.with(|f| *f += 1);
            }
        });
        time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();
        Ok(())
    })
    .await;
    match result {
        Ok(()) | Err(ScopeError::Cancelled) => {}
        Err(error) => return Err(error),
    }
    assert!(flushes.read(|f| *f > 0));
    Ok(())
}
```

`tokio::time::interval` and `tokio::time::sleep` become a loop over
`time::sleep_until` and `time::sleep`, so waits go through Adamantium's clock.
A task that never returns has no value for Rust to infer its type from, so
the spawn names it: `spawn::<(), _>`. Give each scope its own `Cancellation`, since a scope cancels its signal when
it ends. Work that must stop when an outer scope stops can watch a clone of
the outer signal.

### Spawning from inside a task

`t.spawn` needs the scope, and a task can't borrow the scope that runs it. A
task that starts tasks of its own runs its own scope, which finishes before
the task does when the task runs to the end.

Shutdown doesn't reach through nesting. When an outer scope aborts the task,
for example on `run_until` cancellation, the inner scope is dropped rather than
joined: its tasks are asked to abort but not waited for, and blocking closures
registered with it keep running. The outer scope can return while that work is
still stopping. Register work that shutdown must wait for in the outer scope.

Before:

```rust
#[tokio::main]
async fn main() {
    let connection = tokio::spawn(async {
        let mut requests = Vec::new();
        for request in 0..3 {
            requests.push(tokio::spawn(async move { request * 2 }));
        }
        let mut total = 0;
        for request in requests {
            total += request.await.unwrap();
        }
        total
    });
    assert_eq!(connection.await.unwrap(), 6);
}
```

After:

```rust
use adamantium::Tasks;
use std::convert::Infallible;

#[tokio::main]
async fn main() {
    let Ok(total) = Tasks::<Infallible>::run(async |t| {
        let connection = t.spawn(async {
            Tasks::run(async |inner| {
                let requests: Vec<_> = (0..3)
                    .map(|request| inner.spawn(async move { Ok(request * 2) }))
                    .collect();
                let mut total = 0;
                for request in requests {
                    total += inner.value(request).await?;
                }
                Ok(total)
            })
            .await
        });
        t.value(connection).await
    })
    .await;
    assert_eq!(total, 6);
}
```
