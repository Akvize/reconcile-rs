// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Transport`] port and its [`UdpTransport`]/[`InMemoryTransport`] adapters
//! (`ARCHITECTURE.md` §3.2).

use std::hash::Hash;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use socket2::SockRef;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// Connectionless datagram I/O. The engine is written against `Addr = SocketAddr`, so every
/// adapter reuses that address type.
///
/// # Implementing your own
///
/// #297: every type this trait's signature names — `Self::Addr`'s bounds, `io::Result`, the
/// `#[async_trait]` macro itself — is either `std` or re-exported from this crate (or
/// `reconcile`), so an external implementation never has to independently depend on
/// `async-trait` and match its version to this crate's:
///
/// ```
/// use std::io;
/// use std::net::SocketAddr;
///
/// use reconcile_gossip::async_trait;
/// use reconcile_gossip::transport::Transport;
///
/// struct NullTransport;
///
/// #[async_trait]
/// impl Transport for NullTransport {
///     type Addr = SocketAddr;
///
///     async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Self::Addr)> {
///         Ok((buf.len(), self.local_addr()?))
///     }
///
///     async fn send_to(&self, buf: &[u8], _dst: &Self::Addr) -> io::Result<usize> {
///         Ok(buf.len())
///     }
///
///     fn local_addr(&self) -> io::Result<Self::Addr> {
///         Ok("0.0.0.0:0".parse().unwrap())
///     }
/// }
/// ```
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// The peer-address type carried by [`recv_from`](Transport::recv_from) /
    /// [`send_to`](Transport::send_to).
    type Addr: Clone + Eq + Hash + Send + Sync;

    /// Receive one datagram into `buf`, returning the number of bytes read and the sender address.
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Self::Addr)>;

    /// Send one datagram to `dst`, returning the number of bytes written.
    async fn send_to(&self, buf: &[u8], dst: &Self::Addr) -> io::Result<usize>;

    /// The local address this transport is bound to.
    fn local_addr(&self) -> io::Result<Self::Addr>;
}

/// The default [`Transport`] adapter: a tokio UDP socket.
#[derive(Clone, Debug)]
pub struct UdpTransport(Arc<UdpSocket>);

impl UdpTransport {
    /// Wrap an already-bound UDP socket.
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        UdpTransport(socket)
    }

    /// Bind a UDP socket at `addr` and size its kernel send/receive buffers, returning the ready
    /// transport. `recv_buffer_size` / `send_buffer_size` size `SO_RCVBUF` / `SO_SNDBUF`
    /// respectively; `None` leaves the inherited OS default.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the socket cannot be bound to `addr` (e.g. the port is in use).
    pub async fn bind(
        addr: SocketAddr,
        recv_buffer_size: Option<usize>,
        send_buffer_size: Option<usize>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        set_socket_buffers(&socket, recv_buffer_size, send_buffer_size);
        Ok(UdpTransport(Arc::new(socket)))
    }

    /// Borrow the underlying socket (e.g. to tune `SO_RCVBUF` / `SO_SNDBUF`).
    pub fn socket(&self) -> &UdpSocket {
        &self.0
    }
}

/// Apply the requested `SO_RCVBUF` / `SO_SNDBUF` sizes; `None` leaves the OS default.
///
/// The kernel clamps an over-large request rather than failing, so clamping is a `debug`, not a
/// warning. Linux `getsockopt` reports the doubled value, so a honoured request reads back larger
/// than asked.
fn set_socket_buffers(
    socket: &UdpSocket,
    recv_buffer_size: Option<usize>,
    send_buffer_size: Option<usize>,
) {
    let sock = SockRef::from(socket);
    if let Some(size) = recv_buffer_size {
        match sock.set_recv_buffer_size(size) {
            Ok(()) => match sock.recv_buffer_size() {
                Ok(actual) => debug!(
                    "gossip socket SO_RCVBUF: requested {size} B, OS granted {actual} B \
                     (raise net.core.rmem_max if a larger buffer is needed)"
                ),
                Err(e) => debug!("could not read back SO_RCVBUF: {e}"),
            },
            Err(e) => warn!("failed to set gossip socket SO_RCVBUF to {size} B: {e}"),
        }
    }
    if let Some(size) = send_buffer_size {
        match sock.set_send_buffer_size(size) {
            Ok(()) => match sock.send_buffer_size() {
                Ok(actual) => debug!(
                    "gossip socket SO_SNDBUF: requested {size} B, OS granted {actual} B \
                     (raise net.core.wmem_max if a larger buffer is needed)"
                ),
                Err(e) => debug!("could not read back SO_SNDBUF: {e}"),
            },
            Err(e) => warn!("failed to set gossip socket SO_SNDBUF to {size} B: {e}"),
        }
    }
}

#[async_trait]
impl Transport for UdpTransport {
    type Addr = SocketAddr;

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf).await
    }

    async fn send_to(&self, buf: &[u8], dst: &SocketAddr) -> io::Result<usize> {
        self.0.send_to(buf, dst).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

/// An in-process [`Transport`] over a shared [`InMemoryNetwork`]: reliable and FIFO per
/// sender→receiver pair, so convergence is deterministic on a single-threaded runtime. A datagram
/// to an unbound address is dropped, as with UDP.
///
/// Public, not test-gated, so downstream crates can drive a deterministic cluster of their own.
pub use in_memory::{InMemoryNetwork, InMemoryTransport};

mod in_memory {
    use super::*;
    use std::collections::HashMap;

    use parking_lot::Mutex;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
    use tokio::sync::Mutex as AsyncMutex;

    type Datagram = (SocketAddr, Vec<u8>);

    /// A shared in-process datagram fabric. [`bind`](InMemoryNetwork::bind) a
    /// [`InMemoryTransport`] per node onto the same network and they can exchange datagrams.
    #[derive(Clone, Default)]
    pub struct InMemoryNetwork {
        routes: Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Datagram>>>>,
    }

    impl InMemoryNetwork {
        /// An empty fabric with no bound transports. Equivalent to [`Default::default`].
        pub fn new() -> Self {
            InMemoryNetwork::default()
        }

        /// Bind a transport at `addr`. A datagram sent to `addr` by any peer on this network is
        /// delivered to the returned transport's [`recv_from`](Transport::recv_from).
        pub fn bind(&self, addr: SocketAddr) -> InMemoryTransport {
            let (tx, rx) = unbounded_channel();
            self.routes.lock().insert(addr, tx);
            InMemoryTransport {
                network: self.clone(),
                addr,
                rx: AsyncMutex::new(rx),
            }
        }
    }

    /// One endpoint on an [`InMemoryNetwork`].
    pub struct InMemoryTransport {
        network: InMemoryNetwork,
        addr: SocketAddr,
        rx: AsyncMutex<UnboundedReceiver<Datagram>>,
    }

    #[async_trait]
    impl Transport for InMemoryTransport {
        type Addr = SocketAddr;

        async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            let (src, bytes) = self.rx.lock().await.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "in-memory network closed")
            })?;
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok((n, src))
        }

        async fn send_to(&self, buf: &[u8], dst: &SocketAddr) -> io::Result<usize> {
            if let Some(tx) = self.network.routes.lock().get(dst) {
                let _ = tx.send((self.addr, buf.to_vec()));
            }
            Ok(buf.len())
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_delivers_between_endpoints() {
        let net = InMemoryNetwork::new();
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let ta = net.bind(a);
        let tb = net.bind(b);

        ta.send_to(b"hello", &b).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, src) = tb.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(src, a);
        assert_eq!(tb.local_addr().unwrap(), b);
    }

    #[tokio::test]
    async fn in_memory_drops_datagram_to_unknown_address() {
        let net = InMemoryNetwork::new();
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let ta = net.bind(a);
        let unknown: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let n = ta.send_to(b"lost", &unknown).await.unwrap();
        assert_eq!(n, 4);
    }
}
