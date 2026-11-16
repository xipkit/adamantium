//! Serve TCP connections, each in its own task, where one failed connection
//! is logged and counted while the server and the other connections go on.

use adamantium::{Actor, Cancellation, ScopeError, Tasks};
use std::{io, net::SocketAddr};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

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
    let failed = Actor::new(0);
    let shutdown = Cancellation::new();
    let clients = shutdown.clone();

    let served = Tasks::run_until(shutdown, None, async |tasks| {
        tasks.spawn(async move {
            for (request, expected) in [("hello", "HELLO\n"), ("fail", ""), ("again", "AGAIN\n")] {
                let response = ask(address, request).await?;
                if response != expected {
                    return Err(io::Error::other(format!(
                        "{request:?} got {response:?}, expected {expected:?}"
                    )));
                }
                println!("{request:?} -> {response:?}");
            }
            clients.cancel();
            Ok(())
        });
        loop {
            let (stream, peer) = listener.accept().await?;
            let failed = failed.clone();
            tasks.spawn_handled(serve(stream), move |error| {
                eprintln!("connection from {peer} failed: {error}");
                failed.with(|failed| *failed += 1);
            });
        }
    })
    .await;

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
    println!("served every connection; 1 failed and was handled");
    Ok(())
}
