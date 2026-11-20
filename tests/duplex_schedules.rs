#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "test observations, schedule assertions and deliberate execution-boundary misuse"
)]

use std::{
    future::Future,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use adamantium::{
    testing::{
        self, Config, DuplexConfig, DuplexStream, FailureKind, LinkConfig, Replay, TransportFault,
    },
    time,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

fn config() -> Config {
    Config {
        seed: 204,
        iterations: 200,
        max_steps: 20_000,
        ..Config::default()
    }
}

fn check<F, Fut>(factory: F) -> Replay
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()>,
{
    let record = testing::check(config(), factory.clone()).unwrap();
    assert_eq!(testing::check(config(), factory.clone()).unwrap(), record);
    assert_eq!(testing::replay(&record, factory).unwrap(), record);
    record
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

#[test]
fn partial_reads_writes_and_backpressure_transfer_all_bytes_in_order() {
    check(|| async {
        let link = LinkConfig {
            buffer_capacity: 4,
            max_read_chunk: Some(2),
            max_write_chunk: Some(3),
            latency: Duration::ZERO,
            disconnect_after: None,
        };
        let (mut a, mut b) = testing::duplex_with(DuplexConfig::symmetric(link));
        let payload: Vec<u8> = (0_u8..37).collect();
        let expected = payload.clone();

        let workers: Vec<Worker> = vec![
            Box::pin(async move {
                let first = a.write(&payload).await.unwrap();
                assert!(first > 0 && first <= 3);
                a.write_all(&payload[first..]).await.unwrap();
                a.shutdown().await.unwrap();
            }),
            Box::pin(async move {
                let mut received = Vec::new();
                let mut buf = [0_u8; 16];
                loop {
                    let n = b.read(&mut buf).await.unwrap();
                    assert!(n <= 2);
                    if n == 0 {
                        break;
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                assert_eq!(received, expected);
            }),
        ];
        testing::parallel(workers).await;
    });
}

#[test]
fn clock_driven_latency_advances_virtual_time_without_wall_clock_waits() {
    let wall_start = std::time::Instant::now();
    check(|| async {
        let link = LinkConfig {
            buffer_capacity: 64,
            max_read_chunk: Some(5),
            max_write_chunk: Some(5),
            latency: Duration::from_secs(30),
            disconnect_after: None,
        };
        let (mut a, mut b) = testing::duplex_with(DuplexConfig::symmetric(link));
        let start = time::now();

        let workers: Vec<Worker> = vec![
            Box::pin(async move {
                a.write_all(b"hello world").await.unwrap();
                a.shutdown().await.unwrap();
            }),
            Box::pin(async move {
                let mut buf = Vec::new();
                b.read_to_end(&mut buf).await.unwrap();
                assert_eq!(buf, b"hello world");
                assert!(time::now() >= start + Duration::from_secs(30));
            }),
        ];
        testing::parallel(workers).await;
    });
    assert!(wall_start.elapsed() < Duration::from_secs(5));
}

#[test]
fn immediate_disconnect_wakes_reader_waiting_on_in_flight_latency() {
    check(|| async {
        let link = LinkConfig {
            buffer_capacity: 32,
            max_read_chunk: None,
            max_write_chunk: None,
            latency: Duration::from_secs(600),
            disconnect_after: None,
        };
        let (mut a, mut b, control) = testing::duplex_controlled(DuplexConfig::symmetric(link));
        let start = time::now();

        let workers: Vec<Worker> = vec![
            Box::pin(async move {
                a.write_all(b"delayed").await.unwrap();
            }),
            Box::pin(async move {
                let mut buf = [0_u8; 16];
                let err = b.read(&mut buf).await.unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
                assert!(time::now() < start + Duration::from_secs(600));
            }),
            Box::pin(async move {
                time::sleep(Duration::from_secs(1)).await;
                control.disconnect(TransportFault::Error(io::ErrorKind::ConnectionReset));
            }),
        ];
        testing::parallel(workers).await;
    });
}

#[test]
fn framed_request_response_fixture_and_saved_schedule_replay_mid_frame_disconnect() {
    async fn framed_session(disconnect_after: Option<(usize, TransportFault)>) {
        let client_to_server = LinkConfig {
            buffer_capacity: 8,
            max_read_chunk: Some(3),
            max_write_chunk: Some(5),
            latency: Duration::from_millis(25),
            disconnect_after: None,
        };
        let server_to_client = LinkConfig {
            buffer_capacity: 8,
            max_read_chunk: Some(3),
            max_write_chunk: Some(4),
            latency: Duration::from_millis(25),
            disconnect_after,
        };
        let (mut client, mut server) =
            testing::duplex_with(DuplexConfig::new(client_to_server, server_to_client));

        let workers: Vec<Worker> = vec![
            Box::pin(async move {
                write_frame(&mut client, b"ping-one").await.unwrap();
                let first = read_frame(&mut client).await.unwrap();
                assert_eq!(first.as_deref(), Some(b"pong:ping-one".as_slice()));

                write_frame(&mut client, b"ping-two").await.unwrap();
                let second = read_frame(&mut client).await;
                if disconnect_after.is_some() {
                    let err = second.unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
                } else {
                    assert_eq!(
                        second.unwrap().as_deref(),
                        Some(b"pong:ping-two".as_slice())
                    );
                }
                let _ = client.shutdown().await.ok();
            }),
            Box::pin(async move {
                while let Ok(Some(req)) = read_frame(&mut server).await {
                    let mut resp = b"pong:".to_vec();
                    resp.extend_from_slice(&req);
                    if write_frame(&mut server, &resp).await.is_err() {
                        break;
                    }
                }
                let _ = server.shutdown().await.ok();
            }),
        ];
        testing::parallel(workers).await;
    }

    check(|| framed_session(None));

    let disconnect_scenario = || {
        framed_session(Some((
            23,
            TransportFault::Error(io::ErrorKind::ConnectionReset),
        )))
    };

    let record = testing::check(config(), disconnect_scenario).unwrap();
    let path = std::env::temp_dir().join(format!(
        "adamantium-duplex-replay-{}.json",
        std::process::id()
    ));
    record.save(&path).unwrap();
    let loaded = Replay::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(loaded, record);
    assert_eq!(
        testing::replay(&loaded, disconnect_scenario).unwrap(),
        record
    );
}

#[test]
fn concurrent_fault_failure_replays_exact_interleaving_from_saved_schedule() {
    let observed_bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = observed_bytes.clone();

    let scenario = move || {
        let captured = captured.clone();
        async move {
            let link = LinkConfig {
                buffer_capacity: 6,
                max_read_chunk: Some(2),
                max_write_chunk: Some(3),
                latency: Duration::from_millis(10),
                disconnect_after: None,
            };
            let (mut client, mut server, control) =
                testing::duplex_controlled(DuplexConfig::symmetric(link));

            let workers: Vec<Worker> = vec![
                Box::pin(async move {
                    let written = write_frame(&mut client, b"abcdefghij").await;
                    assert!(written.is_ok(), "unexpected write disconnect: {written:?}");
                    let resp = read_frame(&mut client).await;
                    assert!(resp.is_ok(), "unexpected transport disconnect: {resp:?}");
                }),
                Box::pin(async move {
                    if let Ok(Some(req)) = read_frame(&mut server).await {
                        let _ = write_frame(&mut server, &req).await.ok();
                    }
                }),
                Box::pin(async move {
                    control.disconnect(TransportFault::Error(io::ErrorKind::ConnectionReset));
                }),
            ];
            testing::parallel(workers).await;
            captured.lock().unwrap().push(0);
        }
    };

    let failure = testing::check(config(), scenario.clone()).unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Panic);
    let replayed = testing::replay(failure.replay(), scenario).unwrap_err();
    assert_eq!(replayed.kind(), FailureKind::Panic);
    assert_eq!(replayed.replay(), failure.replay());
}

#[test]
fn foreign_execution_use_and_drop_are_rejected() {
    let escaped: Arc<Mutex<Option<DuplexStream>>> = Arc::new(Mutex::new(None));
    let out = escaped.clone();
    testing::check(
        Config {
            iterations: 1,
            ..config()
        },
        move || {
            let out = out.clone();
            async move {
                let (a, _b) = testing::duplex(8);
                *out.lock().unwrap() = Some(a);
            }
        },
    )
    .unwrap();
    let stream = escaped.lock().unwrap().take().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| drop(stream))).is_err());
}
