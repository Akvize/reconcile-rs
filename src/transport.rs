// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Transport`] port — the domain's datagram-I/O boundary — and its default
//! [`UdpTransport`] adapter (`ARCHITECTURE.md` §3.4).
//!
//! The reconciliation engine drives itself over this port instead of calling `tokio::net` directly,
//! so the socket is a substitutable adapter. A test-only [`InMemoryTransport`] delivers datagrams
//! between engines in-process (no real sockets), which is what makes convergence tests
//! deterministic.

use std::hash::Hash;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use socket2::SockRef;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// Abstracts connectionless datagram I/O (send / receive / local address).
///
/// The associated `Addr` is the peer-address type; the default [`UdpTransport`] uses
/// [`SocketAddr`], and the reconciliation engine is written against `Addr = SocketAddr` (its peer,
/// membership and geography bookkeeping are all keyed on IP). A different adapter (e.g.
/// [`InMemoryTransport`]) reuses the same address type so none of that logic changes.
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

/// Apply the requested `SO_RCVBUF` / `SO_SNDBUF` sizes to a gossip socket.
///
/// `setsockopt` never errors for asking too much: the kernel clamps each request to the OS maximum
/// (`net.core.rmem_max` / `wmem_max` on Linux), so a generous default only ever helps. Clamping is
/// therefore expected on an untuned host — **not** a warning condition — so the achieved size is
/// reported at `debug`; an operator who needs the full buffer raises the sysctl (see the README)
/// and can confirm via `/proc/net/snmp` `RcvbufErrors`. Only an actual `setsockopt` failure (which
/// does not happen for a buffer request on a valid socket) is surfaced as a `warn`. A `None` size
/// leaves the inherited OS default untouched.
///
/// Note: on Linux `getsockopt` reports the *doubled* value (bookkeeping overhead), so a fully
/// honoured request reads back larger than asked.
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

/// An in-process [`Transport`]: datagrams are routed between transports sharing an
/// [`InMemoryNetwork`], with no real sockets. Delivery is reliable and FIFO per (sender→receiver)
/// pair, which — under a single-threaded runtime — makes convergence deterministic. A datagram to
/// an unknown/closed address is dropped, exactly like UDP.
///
/// Exposed (not test-gated) so downstream crates can test *their own* application against a
/// deterministic cluster, which is the second of the two uses that earn `Transport` a public
/// injection point — see `ARCHITECTURE.md` §7 D2.
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
        /// Create an empty network.
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
            // Deliver if the destination is bound; otherwise drop silently, like UDP to nowhere.
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
        // No panic, no delivery: reports the bytes as "sent" then dropped.
        let n = ta.send_to(b"lost", &unknown).await.unwrap();
        assert_eq!(n, 4);
    }
}
