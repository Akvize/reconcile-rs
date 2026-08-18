// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use socket2::SockRef;

use crate::transport::UdpTransport;

async fn bound(addr: &str, recv: Option<usize>, send: Option<usize>) -> UdpTransport {
    UdpTransport::bind(addr.parse().unwrap(), recv, send)
        .await
        .expect("bind failed")
}

/// The receive-buffer knob must size the socket. Asserted as monotonicity, not an absolute
/// count: the kernel clamps `SO_RCVBUF` to a per-host `net.core.rmem_max`.
#[tokio::test]
async fn recv_buffer_size_is_configurable() {
    let big = bound("127.0.0.90:0", Some(4 * 1024 * 1024), None).await;
    // An explicit, tiny request — well below any plausible rmem_max, so it is honoured as-is.
    let small = bound("127.0.0.91:0", Some(8 * 1024), None).await;

    let big_buf = SockRef::from(big.socket()).recv_buffer_size().unwrap();
    let small_buf = SockRef::from(small.socket()).recv_buffer_size().unwrap();

    assert!(
        big_buf > small_buf,
        "the multi-MiB request ({big_buf} B) should exceed an explicitly tiny buffer \
         ({small_buf} B)"
    );
}

/// `None` opts out of the tuning entirely, leaving the inherited OS default: the call path does
/// not panic and the socket has a positive receive buffer.
#[tokio::test]
async fn recv_buffer_size_none_leaves_os_default() {
    let t = bound("127.0.0.92:0", None, None).await;
    let buf = SockRef::from(t.socket()).recv_buffer_size().unwrap();
    assert!(buf > 0, "a socket always has a positive receive buffer");
}
