#![allow(dead_code, unused_must_use)]
use metal::{Actor, KeyedLimit, Limit, Tasks, blocking};
use std::{fs as disk, io::{Cursor, Read, Write}, time::Duration};

type State = Actor<u8>;
macro_rules! sleep { () => { std::thread::sleep(Duration::ZERO) }; }

pub async fn blocking_calls(file: &mut std::fs::File, socket: &mut std::net::TcpStream, child: &mut std::process::Child) {
    disk::read("missing"); // blocking
    let read = disk::read::<&str>;
    read("missing"); // blocking
    std::fs::File::open("missing"); // blocking
    file.read(&mut [0]); // blocking
    Read::read(file, &mut [0]); // blocking
    socket.write_all(b"hello"); // blocking
    std::net::TcpStream::connect("localhost:1"); // blocking
    std::net::TcpListener::bind("localhost:0"); // blocking
    std::net::UdpSocket::bind("localhost:0"); // blocking
    child.wait(); // blocking
    std::process::Command::new("true").status(); // blocking
    std::path::Path::new("missing").exists(); // blocking
    sleep!(); // blocking
    let closure = || disk::read("missing"); // blocking
    let alias = closure;
    alias();
    (|| { disk::read("missing"); })(); // blocking
    Some(1).map(|_| disk::read("missing")); // blocking
    [1].iter().for_each(|_| { disk::read("missing"); }); // blocking
    let _deferred = || disk::read("missing");
    Cursor::new(b"hi").read(&mut [0]);
    blocking(|| disk::read("missing")).await;
    blocking({ disk::read("missing"); || () }).await; // blocking
    Tasks::<()>::run(async |tasks| {
        tasks.spawn_blocking(|| { disk::read("missing"); Ok(()) });
        tasks.spawn_blocking({ disk::read("missing"); || Ok(()) }); // blocking
        Ok(())
    }).await;
}

pub fn async_boundaries() {
    let _future = async { disk::read("missing"); }; // blocking
    let _closure = async || { disk::read("missing"); }; // blocking
    disk::read("missing");
}

pub fn nesting(a: &State, b: &State) {
    a.with(|_| { b.with(|_| ()); }); // nested
    a.with(|_| { b.read(|_| ()); }); // nested
    a.read(|_| { b.with(|_| ()); }); // nested
    a.read(|_| { b.read(|_| ()); }); // nested
    a.with(|_| { a.snapshot(); }); // nested
    Actor::read(a, |_| { State::with(b, |_| ()); }); // nested
    a.read(|_| Some(1).map(|_| b.snapshot())); // nested
    a.with(|_| { let c = || b.snapshot(); c(); }); // nested
    a.with(|_| { let _deferred = || b.snapshot(); });
    let _future = a.with(|_| async { b.snapshot() });
    let _closure = a.read(|_| || b.snapshot());
    a.with({ b.snapshot(); |_| () });
    a.snapshot();
}

pub async fn combined(a: &State, b: &State) {
    a.read(|_| { disk::read("missing"); }); // actor-blocking
    let _future = a.read(|_| async { b.snapshot(); disk::read("missing"); }); // blocking
}

struct Unrelated;
impl Unrelated {
    fn read(&self, f: impl FnOnce()) { f(); }
    fn with(&self, f: impl FnOnce()) { f(); }
    fn snapshot(&self) {}
}
fn unrelated_names(a: &State) {
    let other = Unrelated;
    a.read(|_| other.snapshot());
    other.with(|| other.read(|| ()));
}

macro_rules! nested { ($actor:expr) => { $actor.snapshot() }; }
fn actor_macro(a: &State, b: &State) {
    a.read(|_| nested!(b)); // nested
}

async fn closure_defaults(a: &State) {
    let callback = || { disk::read("missing"); };
    let _returned = None::<u8>.map_or(callback, |_| callback);
    let _returned = [0].iter().fold(callback, |acc, _| acc);
    let _future = a.with(|_| async { a.snapshot() });
    blocking(|| { let _future = async { disk::read("missing"); }; }).await; // blocking
}

trait Harmless {
    fn write_all(&mut self);
}
impl Harmless for std::fs::File {
    fn write_all(&mut self) {}
}
async fn unrelated_trait(file: &mut std::fs::File) {
    Harmless::write_all(file);
}

async fn local_allow() {
    #[allow(adamantium_blocking_in_async)]
    { disk::read("missing"); }
}

#[cfg(feature = "extra")]
pub async fn extra() { disk::read("missing"); } // feature-blocking

async fn repeated_callbacks() {
    let f0 = || { disk::read("missing"); }; // blocking
    let f1 = || { f0(); f0(); };
    let f2 = || { f1(); f1(); };
    let f3 = || { f2(); f2(); };
    let f4 = || { f3(); f3(); };
    let f5 = || { f4(); f4(); };
    let f6 = || { f5(); f5(); };
    let f7 = || { f6(); f6(); };
    let f8 = || { f7(); f7(); };
    let f9 = || { f8(); f8(); };
    let f10 = || { f9(); f9(); };
    let f11 = || { f10(); f10(); };
    let f12 = || { f11(); f11(); };
    let f13 = || { f12(); f12(); };
    let f14 = || { f13(); f13(); };
    let f15 = || { f14(); f14(); };
    let f16 = || { f15(); f15(); };
    let f17 = || { f16(); f16(); };
    let f18 = || { f17(); f17(); };
    let f19 = || { f18(); f18(); };
    let f20 = || { f19(); f19(); };
    let f21 = || { f20(); f20(); };
    let f22 = || { f21(); f21(); };
    let f23 = || { f22(); f22(); };
    let f24 = || { f23(); f23(); };
    let f25 = || { f24(); f24(); };
    f25();
}

async fn other_operations(mut file: std::fs::File, socket: std::net::UdpSocket, handle: std::thread::JoinHandle<()>) {
    use std::io::Seek;
    file.rewind(); // blocking
    std::fs::OpenOptions::new().open("missing"); // blocking
    std::process::Command::new("true").output(); // blocking
    std::thread::park(); // blocking
    std::thread::park_timeout(Duration::ZERO); // blocking
    handle.join(); // blocking
    socket.recv(&mut [0]); // blocking
}

async fn const_arguments() {
    const fn length(text: &str) -> usize { text.len() }
    const NAME: [u8; length("name")] = [0; length("name")];
    let _: [u8; length("value")] = [0; length("value")];
    let _ = NAME;
}

async fn custom_work() {}

async fn race_with_timer(
    ch: &metal::Channel<()>,
    rx: &metal::Receiver<()>,
    reply_rx: &metal::ReplyReceiver<()>,
    reply: &metal::Reply<()>,
    watcher: &mut metal::Watcher<u8>,
    cancel: &metal::Cancellation,
) {
    use metal::{Race, race, time};
    let delay = Duration::from_millis(1);
    let deadline = time::now() + delay;

    // Flagged: timer arm paired with non-drop-safe futures.
    race((time::sleep(delay), custom_work())).await; // race
    race((async { time::sleep(delay).await; 1 }, async { custom_work().await; 2 })).await; // race
    race((async { time::sleep_until(deadline).await; 1 }, async { custom_work().await; 2 })).await; // race
    (time::sleep(delay), custom_work()).race().await; // race
    race((time::sleep(delay), async { rx.receive().await; }, async { custom_work().await })).await; // race
    race((async { time::sleep(delay).await }, async { rx.receive().await; custom_work().await })).await; // race
    race((time::sleep(delay), async { (async { rx.receive().await; }).await; })).await; // race

    // Allowed: every arm is on the documented drop-safe list.
    race((time::sleep(delay), async { ch.receive().await; })).await;
    race((time::sleep(delay), async { rx.receive().await; })).await;
    race((time::sleep(delay), async { reply_rx.receive().await; })).await;
    race((time::sleep(delay), reply.closed())).await;
    race((time::sleep(delay), async { watcher.receive().await; })).await;
    race((time::sleep(delay), async { watcher.wait_for(|v| *v > 0).await; })).await;
    race((time::sleep(delay), time::sleep(delay))).await;
    race((time::sleep(delay), time::sleep_until(deadline))).await;
    race((time::sleep(delay), cancel.cancelled())).await;
    race((async { time::sleep(delay).await; 1 }, async { rx.receive().await; 2 })).await;

    // Allowed: no timer arm, or explicitly allowed.
    race((custom_work(), custom_work())).await;
    #[allow(adamantium_race_with_timer)]
    race((time::sleep(delay), custom_work())).await;
}

// Helpers: an effect is reported where its helper is called, or where the
// helper's future is awaited, in the caller's context.
fn load() -> std::io::Result<Vec<u8>> { disk::read("missing") }
fn load_twice() { load(); load(); }
fn offload() { let _ = blocking(|| load()); }
fn construct() -> impl std::future::Future<Output = ()> { async { load(); } } // blocking
async fn load_async() { load(); } // blocking
fn retry<F: FnMut()>(mut f: F) { f(); }
fn retry_twice<F: FnMut()>(mut f: F) { retry(&mut f); retry(f); }
fn walk(n: u32) { if n == 0 { load(); } else { walk(n - 1); } }
fn count(n: u32) -> u32 { if n == 0 { 0 } else { count(n - 1) + 1 } }
fn even(n: u32) { if n == 0 { load(); } else { odd(n - 1); } }
fn odd(n: u32) { if n != 0 { even(n - 1); } }
fn is_even(n: u32) -> bool { n == 0 || is_odd(n - 1) }
fn is_odd(n: u32) -> bool { n != 0 && is_even(n - 1) }

pub async fn blocking_helpers() {
    load(); // blocking
    load_twice(); // blocking
    walk(3); // blocking
    odd(3); // blocking
    retry(|| { load(); }); // blocking
    retry(load_twice); // blocking
    retry_twice(|| { load(); }); // blocking
    Some("missing").map(disk::read); // blocking
    blocking({ load(); || () }).await; // blocking
    count(3);
    is_even(3);
    offload();
    blocking(|| load()).await;
    blocking(|| walk(3)).await;
    let _later = construct();
    construct().await;
    let _closure = || load();
    let _future = load_async();
    load_async().await;
}

pub fn sync_helpers() {
    load();
    walk(3);
    odd(3);
    retry(|| { load(); });
}

fn peek(b: &State) -> u8 { b.snapshot() }
fn peek_deep(b: &State, n: u32) -> u8 { if n == 0 { peek(b) } else { peek_deep(b, n - 1) } }
fn deferred(b: &State) -> impl Fn() -> u8 + '_ { move || b.snapshot() }
fn update<F: FnOnce(&mut u8)>(a: &State, f: F) { a.with(f); }
async fn peek_async(b: &State) -> u8 { b.snapshot() }
fn offload_nested(a: State, b: State) {
    let _ = blocking(move || a.with(|_| { peek(&b); })); // nested
}

pub fn actor_helpers(a: &State, b: &State) {
    a.with(|_| { peek(b); }); // nested
    a.read(|_| peek_deep(b, 3)); // nested
    update(a, |_| { b.snapshot(); }); // nested
    a.with(|_| update(b, |_| ())); // nested
    a.with(|_| { count(3); });
    a.with(|_| { let _later = deferred(b); });
    a.with(|_| { let _future = peek_async(b); });
    update(a, |_| ());
    peek(b);
    peek_deep(b, 3);
    let _ = blocking({ let a = a.clone(); move || { peek(&a); } });
}

fn save(_: &mut u8) { disk::write("missing", b""); }
fn persist(a: &State) { a.with(|_| { load(); }); } // actor-blocking
fn status() -> bool { std::process::Command::new("true").status().is_ok() }

pub fn actor_blocking(a: &State, socket: &mut std::net::TcpStream) {
    a.with(|_| { disk::read("missing"); }); // actor-blocking
    a.read(|_| socket.write_all(b"hello")); // actor-blocking
    a.with(|_| sleep!()); // actor-blocking
    a.read(|_| { load(); }); // actor-blocking
    a.with(|_| walk(3)); // actor-blocking
    a.read(|_| odd(3)); // actor-blocking
    a.with(|_| status()); // actor-blocking
    a.with(save); // actor-blocking
    update(a, |_| { load(); }); // actor-blocking
    a.with(|_| { retry(|| { load(); }); }); // actor-blocking
    a.with(|_| Some(1).map(|_| load())); // actor-blocking
    a.read(|_| { let c = || load(); c(); }); // actor-blocking
    persist(a);
    // Allowed: the I/O runs before or after the callback, is handed to a
    // blocking thread, or is only constructed inside it.
    let text = disk::read("missing");
    a.with(|state| *state = text.map_or(0, |bytes| bytes.len() as u8));
    let value = a.snapshot();
    disk::write("missing", [value]);
    a.with(|_| { let _ = blocking(|| load()); });
    a.with(|_| offload());
    a.with(|_| { let _deferred = || load(); });
    a.with(|_| { let _later = construct(); });
    a.with(|_| Cursor::new(b"hi").read(&mut [0]));
    a.read(|_| { count(3); is_even(3); });
    Unrelated.with(|| { load(); });
}

pub async fn actor_blocking_async(a: &State) {
    a.with(|_| { load(); }); // actor-blocking
    persist(a); // blocking
    a.with(|_| { let _ = blocking(|| load()); });
}

async fn enter(l: &Limit) { l.run(|| async {}).await; }
async fn enter_keyed(k: &KeyedLimit<u8>) { k.run(1, || async {}).await; }
async fn descend(l: &Limit, n: u32) { if n == 0 { enter(l).await } else { Box::pin(descend(l, n - 1)).await } }
async fn countdown(n: u32) -> u32 { if n == 0 { 0 } else { Box::pin(countdown(n - 1)).await + 1 } }
async fn ping(l: &Limit, n: u32) { if n == 0 { enter(l).await } else { Box::pin(pong(l, n - 1)).await } }
async fn pong(l: &Limit, n: u32) { if n != 0 { Box::pin(ping(l, n - 1)).await } }
async fn guarded<F: std::future::Future<Output = ()>>(l: &Limit, f: F) { l.run(|| f).await }
async fn guarded_work<W: FnOnce() -> Fut, Fut: std::future::Future<Output = ()>>(l: &Limit, work: W) { l.run(work).await }
fn enter_sync(l: &Limit) -> impl std::future::Future<Output = ()> + '_ { l.run(|| async {}) }

pub async fn limit_helpers(outer: &Limit, inner: &Limit, keys: &KeyedLimit<u8>) {
    outer.run(|| inner.run(|| async {})).await; // limit
    outer.run(|| async { inner.run(|| async {}).await; }).await; // limit
    outer.run(|| enter(inner)).await; // limit
    outer.run(|| async { enter(inner).await }).await; // limit
    outer.run(|| descend(inner, 3)).await; // limit
    outer.run(|| pong(inner, 3)).await; // limit
    keys.run(1, || enter_keyed(keys)).await; // limit
    keys.run(1, || async { outer.run(|| async {}).await }).await; // limit
    guarded(outer, inner.run(|| async {})).await; // limit
    guarded_work(outer, || inner.run(|| async {})).await; // limit
    outer.run(|| enter_sync(inner)).await; // limit
    outer.run(|| keys.run(1, || async {})).await;
    outer.run(|| enter_keyed(keys)).await;
    outer.run(|| async { let _later = enter(inner); }).await;
    outer.run(|| async { countdown(3).await; }).await;
    outer.run(|| countdown(3)).await;
    guarded(outer, enter_keyed(keys)).await;
    guarded_work(outer, || keys.run(1, || async {})).await;
    outer.run(|| async { let _later = enter_sync(inner); }).await;
    enter_sync(inner).await;
    descend(inner, 3).await;
    let _unpolled = outer.run(|| enter(inner));
}

trait Source { fn fetch(&self); }
struct Disk;
impl Source for Disk { fn fetch(&self) { load(); } }
fn through_generic<S: Source>(source: &S) { source.fetch(); }
struct Stalls;
impl std::future::Future for Stalls {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        load();
        std::task::Poll::Ready(())
    }
}

pub async fn dispatch(source: &dyn Source, pointer: fn()) {
    Disk.fetch(); // blocking
    Source::fetch(&Disk); // blocking
    Stalls.await; // blocking
    // Not followed: generic trait calls inside helpers, dynamic dispatch and
    // function pointers.
    through_generic(&Disk);
    source.fetch();
    pointer();
}
