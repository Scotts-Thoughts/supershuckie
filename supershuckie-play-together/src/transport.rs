//! The socket abstraction the sessions run on. [`TcpTransport`] is the only real implementation;
//! tests wrap it to drop or delay bytes.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Makes listeners and outgoing connections.
pub trait Transport: Send + Sync + 'static {
    /// The listener type.
    type Listener: Listener<Connection = Self::Connection>;
    /// The connection type.
    type Connection: Connection;

    /// Listen on `bind_address:port` (`port` 0 picks a free port).
    fn listen(&self, bind_address: &str, port: u16) -> io::Result<Self::Listener>;

    /// Connect to one resolved address within `timeout`.
    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<Self::Connection>;
}

/// A bound listening socket.
pub trait Listener: Send + 'static {
    /// The connection type.
    type Connection: Connection;

    /// Where it listens.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Accept a pending connection without blocking; `Ok(None)` when there is none.
    fn try_accept(&self) -> io::Result<Option<Self::Connection>>;
}

/// One established connection, split into a read half and a write half for two threads.
pub trait Connection: Send + Sync + 'static {
    /// The read half.
    type Read: Read + Send + 'static;
    /// The write half.
    type Write: Write + Send + 'static;

    /// Something to name the peer by in logs and errors (its address).
    fn peer_label(&self) -> String;

    /// Set the socket timeouts both halves use.
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()>;

    /// Clone the socket into its two halves (`try_clone`).
    fn split(&self) -> io::Result<(Self::Read, Self::Write)>;

    /// Shut the socket down in both directions, unblocking whichever thread is inside a read or
    /// write. Idempotent; errors are ignored.
    fn shutdown(&self);
}

/// Plain TCP with `TCP_NODELAY`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpTransport;

/// A non-blocking [`TcpListener`].
#[derive(Debug)]
pub struct TcpListenerHandle {
    listener: TcpListener,
}

/// A [`TcpStream`] with its halves cloned on demand.
#[derive(Debug)]
pub struct TcpConnection {
    stream: TcpStream,
}

impl Transport for TcpTransport {
    type Listener = TcpListenerHandle;
    type Connection = TcpConnection;

    fn listen(&self, bind_address: &str, port: u16) -> io::Result<Self::Listener> {
        let listener = TcpListener::bind((bind_address, port))?;
        listener.set_nonblocking(true)?;
        Ok(TcpListenerHandle { listener })
    }

    fn connect(&self, address: SocketAddr, timeout: Duration) -> io::Result<Self::Connection> {
        let stream = TcpStream::connect_timeout(&address, timeout)?;
        TcpConnection::new(stream)
    }
}

impl Listener for TcpListenerHandle {
    type Connection = TcpConnection;

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn try_accept(&self) -> io::Result<Option<Self::Connection>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                Ok(Some(TcpConnection::new(stream)?))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl TcpConnection {
    /// Wrap a connected stream, enabling `TCP_NODELAY`.
    pub fn new(stream: TcpStream) -> io::Result<TcpConnection> {
        stream.set_nodelay(true)?;
        Ok(TcpConnection { stream })
    }

    /// The underlying stream.
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }
}

impl Connection for TcpConnection {
    type Read = TcpStream;
    type Write = TcpStream;

    fn peer_label(&self) -> String {
        match self.stream.peer_addr() {
            Ok(addr) => addr.to_string(),
            Err(_) => "unknown peer".to_owned(),
        }
    }

    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(read)?;
        self.stream.set_write_timeout(write)
    }

    fn split(&self) -> io::Result<(Self::Read, Self::Write)> {
        Ok((self.stream.try_clone()?, self.stream.try_clone()?))
    }

    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// Resolve `host:port` to every address it names, in order.
pub(crate) fn resolve(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok((host, port).to_socket_addrs()?.collect())
}
