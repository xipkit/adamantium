//! Serve TCP connections in a scope, with a concurrency limit, per-connection
//! error handling, and clean shutdown on a signal or a deadline.

use adamantium::{Actor, Cancellation, Limit, ScopeError, Tasks, concurrently, time};
use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

const MAX_CONNECTIONS: usize = 4;

/// Answers one line with the same line in upper case. A request of `fail`
/// is a protocol error, which ends this connection only.
async fn serve(stream: TcpStream) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut request = String::new();
    BufReader::new(reader).read_line(&mut request).await?;
    let request = request.trim_end();
    if request == "fail" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported request {request:?}"),
        ));
    }
    writer
        .write_all(format!("{}\n", request.to_uppercase()).as_bytes())
        .await
}

/// Sends `request` and returns the whole response, which is empty when the
/// server closed the connection without answering.
async fn ask(address: SocketAddr, request: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(format!("{request}\n").as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let connections = Limit::new(MAX_CONNECTIONS);
    let failed = Actor::new(0usize);
    let shutdown = Cancellation::new();
    let clients = shutdown.clone();
    let deadline = Some(time::now() + Duration::from_secs(30));

    let ((), served) = tokio::try_join!(shutdown.cancel_on_signal(), async {
        Ok(Tasks::run_until(shutdown.clone(), deadline, async |tasks| {
            tasks.spawn(async move {
                concurrently(0..100, 16, |_| async move {
                    let response = ask(address, "hello").await?;
                    if response != "HELLO\n" {
                        return Err(io::Error::other(format!(
                            "\"hello\" got {response:?}, expected \"HELLO\\n\""
                        )));
                    }
                    Ok(())
                })
                .await?;
                let response = ask(address, "fail").await?;
                if !response.is_empty() {
                    return Err(io::Error::other(format!(
                        "\"fail\" got {response:?}, expected an empty response"
                    )));
                }
                concurrently(0..100, 16, |_| async move {
                    let response = ask(address, "again").await?;
                    if response != "AGAIN\n" {
                        return Err(io::Error::other(format!(
                            "\"again\" got {response:?}, expected \"AGAIN\\n\""
                        )));
                    }
                    Ok(())
                })
                .await?;
                clients.cancel();
                Ok(())
            });
            loop {
                let (stream, peer) = listener.accept().await?;
                let connections = connections.clone();
                let failed = failed.clone();
                tasks.spawn_handled(
                    async move { connections.run(|| serve(stream)).await },
                    move |error| {
                        eprintln!("connection from {peer} failed: {error}");
                        failed.with(|failed| *failed += 1);
                    },
                );
                if tasks.len() > 64 {
                    return Err(io::Error::other(
                        "finished connection tasks were not reaped",
                    ));
                }
            }
        })
        .await)
    })?;

    match served {
        Err(ScopeError::Cancelled) => {}
        Err(ScopeError::Failed(error)) => return Err(error),
        Ok(()) | Err(ScopeError::DeadlineExceeded) => {
            return Err(io::Error::other("the server ended without being cancelled"));
        }
    }
    let failed = failed.read(|failed| *failed);
    if failed != 1 {
        return Err(io::Error::other(format!(
            "{failed} connections failed, expected 1"
        )));
    }
    println!("served 201 connections; 1 failed and was handled");
    Ok(())
}
