#![allow(dead_code, unused_must_use)]
use metal::{Actor, Tasks, blocking};
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
    a.read(|_| { disk::read("missing"); }); // blocking
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
