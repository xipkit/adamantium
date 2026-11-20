use adamantium::{AsyncActor, Reply, Tasks};
use std::io;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct Request {
    bytes: Vec<u8>,
    reply: Reply<Vec<u8>>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    Tasks::run(async |tasks| {
        tasks.spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let (mut reader, mut writer) = stream.split();
            tokio::io::copy(&mut reader, &mut writer).await?;
            Ok(())
        });
        let connection = TcpStream::connect(address).await?;
        let actor = AsyncActor::new(tasks, connection, 2, |connection, request: Request| {
            Box::pin(async move {
                connection.write_all(&request.bytes).await?;
                let mut response = vec![0; request.bytes.len()];
                connection.read_exact(&mut response).await?;
                drop(request.reply.send(response));
                Ok(())
            })
        });

        let mut replies = Vec::new();
        for text in ["first exchange", "second exchange", "third exchange"] {
            let (reply, response) = Reply::pair();
            actor
                .send(Request {
                    bytes: text.as_bytes().to_vec(),
                    reply,
                })
                .await
                .map_err(|_| io::Error::other("mailbox closed"))?;
            replies.push((text, response));
        }
        actor.close();
        for (expected, response) in replies {
            let response = response
                .receive()
                .await
                .ok_or_else(|| io::Error::other("worker ended without a reply"))?;
            if response != expected.as_bytes() {
                return Err(io::Error::other("incorrect echo response"));
            }
            println!("echo: {expected}");
        }
        Ok(())
    })
    .await?;
    println!("mailbox drained; connection and server shut down");
    Ok(())
}
