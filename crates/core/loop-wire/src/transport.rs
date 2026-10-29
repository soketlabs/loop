//! Transports carry whole messages between two peers.
//!
//! A [`Transport`] is a pair of channels of JSON texts, one message each.
//! Adapters turn a byte stream or a socket into one: [`lines`] for stdio and
//! Unix sockets (one message per line), [`memory`] for two peers in one
//! process. Dropping the outgoing side closes the underlying stream's output.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Longest message accepted from the peer. Longer ones are dropped so a
/// misbehaving peer cannot exhaust memory.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// One end of a connection: the messages that arrive, and a sender for the
/// messages to send.
#[derive(Debug)]
pub struct Transport {
    /// Messages from the peer, in order. Closes when the peer's side ends.
    pub incoming: mpsc::UnboundedReceiver<String>,
    /// Messages to the peer. Dropping it closes the output.
    pub outgoing: mpsc::UnboundedSender<String>,
}

/// Two connected ends in one process.
pub fn memory() -> (Transport, Transport) {
    let (a_tx, a_rx) = mpsc::unbounded_channel();
    let (b_tx, b_rx) = mpsc::unbounded_channel();
    (
        Transport {
            incoming: b_rx,
            outgoing: a_tx,
        },
        Transport {
            incoming: a_rx,
            outgoing: b_tx,
        },
    )
}

/// One message per line over a byte stream (stdio, a Unix socket). Blank
/// lines are skipped; lines over [`MAX_MESSAGE_BYTES`] are dropped.
pub fn lines<R, W>(reader: R, writer: W) -> Transport
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (incoming_tx, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut to_write) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(mut message) = to_write.recv().await {
            message.push('\n');
            if writer.write_all(message.as_bytes()).await.is_err() || writer.flush().await.is_err()
            {
                break;
            }
        }
        // Closing the output tells the peer nothing more is coming.
        let _ = writer.shutdown().await;
    });
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_line_capped(&mut reader, &mut line).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "connection read failed");
                    return;
                }
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.is_empty() && incoming_tx.send(text.to_owned()).is_err() {
                return;
            }
        }
    });
    Transport { incoming, outgoing }
}

/// Read one line, dropping lines over [`MAX_MESSAGE_BYTES`]. Returns the
/// bytes read including the newline; 0 at end of stream.
async fn read_line_capped<R>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    let mut total = 0;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        let used = chunk.len();
        if !oversized && line.len() + used <= MAX_MESSAGE_BYTES {
            line.extend_from_slice(chunk);
        } else if !oversized {
            oversized = true;
            line.clear();
            tracing::warn!("dropping a message larger than {MAX_MESSAGE_BYTES} bytes");
        }
        reader.consume(used);
        total += used;
        if done {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn lines_split_and_join_messages_across_partial_writes() {
        let (ours, theirs) = duplex(64);
        let (read, write) = tokio::io::split(ours);
        let mut transport = lines(read, write);
        let (mut their_read, mut their_write) = tokio::io::split(theirs);

        let burst = "{\"a\":1}\n\n{\"b\":2}\n";
        let (first, second) = burst.split_at(5);
        their_write.write_all(first.as_bytes()).await.unwrap();
        their_write.write_all(second.as_bytes()).await.unwrap();
        assert_eq!(transport.incoming.recv().await.unwrap(), "{\"a\":1}");
        assert_eq!(transport.incoming.recv().await.unwrap(), "{\"b\":2}");

        transport.outgoing.send("{\"c\":3}".into()).unwrap();
        drop(transport.outgoing);
        let mut written = String::new();
        their_read.read_to_string(&mut written).await.unwrap();
        assert_eq!(
            written, "{\"c\":3}\n",
            "dropping the sender closes the output"
        );
    }

    #[tokio::test]
    async fn memory_ends_are_crossed() {
        let (mut a, mut b) = memory();
        a.outgoing.send("ping".into()).unwrap();
        assert_eq!(b.incoming.recv().await.unwrap(), "ping");
        b.outgoing.send("pong".into()).unwrap();
        assert_eq!(a.incoming.recv().await.unwrap(), "pong");
        drop(a);
        assert!(b.incoming.recv().await.is_none());
    }
}
