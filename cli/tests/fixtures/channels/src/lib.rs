#![allow(dead_code)]

use async_channel::unbounded as async_unbounded;
use crossbeam_channel::unbounded as crossbeam_unbounded;
use flume::unbounded as flume_unbounded;
use std::sync::mpsc::channel as std_unbounded;
use tokio::sync::mpsc::unbounded_channel as tokio_unbounded;

fn unbounded() {
    drop(std::sync::mpsc::channel::<u8>()); // disallowed
    drop(std_unbounded::<u8>()); // disallowed
    drop(tokio::sync::mpsc::unbounded_channel::<u8>()); // disallowed
    drop(tokio_unbounded::<u8>()); // disallowed
    drop(async_channel::unbounded::<u8>()); // disallowed
    drop(async_unbounded::<u8>()); // disallowed
    drop(flume::unbounded::<u8>()); // disallowed
    drop(flume_unbounded::<u8>()); // disallowed
    drop(crossbeam_channel::unbounded::<u8>()); // disallowed
    drop(crossbeam_unbounded::<u8>()); // disallowed
}

fn bounded() {
    drop(tokio::sync::mpsc::channel::<u8>(1));
    drop(async_channel::bounded::<u8>(1));
    drop(flume::bounded::<u8>(1));
    drop(crossbeam_channel::bounded::<u8>(1));
    drop(adamantium::Channel::<u8>::bounded(1));
}
