//! Connection-level protections of `HttpServer`: silent / slow sockets are closed after the
//! header-read timeout, and per-peer / total caps refuse extra sockets at accept time
//! (regression: `axum::serve` ran without a timer, so an incomplete request head held a
//! socket forever and one address could pile up unlimited sockets).

use space3d_net::{ConnectionLimits, HttpServer};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn server(limits: ConnectionLimits) -> space3d_net::ServerHandle {
    HttpServer::new()
        .get_json("/hello", || "hi")
        .connection_limits(limits)
        .start()
        .expect("bind")
}

/// Whether the server closed `s` (EOF or reset) within `within`.
fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    s.set_read_timeout(Some(within)).unwrap();
    let mut buf = [0u8; 256];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue, // an error response, then the close
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return false;
            }
            Err(_) => return true,
        }
    }
}

#[test]
fn silent_and_slow_request_heads_are_closed() {
    let srv = server(ConnectionLimits {
        header_read_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    let started = Instant::now();
    // Never sends a byte.
    let mut idle = TcpStream::connect(srv.local_addr()).unwrap();
    // Sends an incomplete head, then goes silent (slowloris).
    let mut slow = TcpStream::connect(srv.local_addr()).unwrap();
    slow.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n").unwrap();
    assert!(closed_within(&mut idle, Duration::from_secs(3)), "idle socket closed");
    assert!(closed_within(&mut slow, Duration::from_secs(3)), "slow head closed");
    assert!(started.elapsed() < Duration::from_secs(3));
    crate::wait_until("slots released", || srv.connections().open() == 0);
    // A normal request still works.
    let mut ok = TcpStream::connect(srv.local_addr()).unwrap();
    ok.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    ok.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    ok.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
}

#[test]
fn per_peer_and_total_caps_refuse_extra_sockets() {
    let srv = server(ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        max_per_peer: 4,
        ..Default::default()
    });
    let mut held: Vec<TcpStream> = (0..4).map(|_| TcpStream::connect(srv.local_addr()).unwrap()).collect();
    crate::wait_until("4 counted", || srv.connections().open() == 4);
    // The fifth socket from the same address is closed right away.
    let mut extra = TcpStream::connect(srv.local_addr()).unwrap();
    assert!(
        closed_within(&mut extra, Duration::from_secs(3)),
        "fifth socket refused"
    );
    crate::wait_until("refusal counted", || srv.connections().refused() >= 1);
    assert_eq!(srv.connections().open(), 4);
    // Closing one frees a slot.
    drop(held.pop());
    crate::wait_until("slot freed", || srv.connections().open() == 3);
    let mut again = TcpStream::connect(srv.local_addr()).unwrap();
    again
        .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    again.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    again.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");

    // Total cap (exempt peer key: only the global limit applies).
    let srv = server(ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        max_connections: 2,
        max_per_peer: 1,
        peer_key: std::sync::Arc::new(|_| None),
    });
    let _a = TcpStream::connect(srv.local_addr()).unwrap();
    let _b = TcpStream::connect(srv.local_addr()).unwrap();
    crate::wait_until("2 counted", || srv.connections().open() == 2);
    let mut c = TcpStream::connect(srv.local_addr()).unwrap();
    assert!(closed_within(&mut c, Duration::from_secs(3)), "over the total cap");
}
