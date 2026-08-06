mod common;

use common::*;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::Ordering;
use std::time::Duration;

fn proxy_config(bind_port: u16, connect_port: u16, protocol: &str) -> String {
    format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "{}"
"#,
        bind_port, connect_port, protocol
    )
}

#[test]
fn tcp_to_udp_forward() {
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, echo.addr.port(), "tcptoudp"));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"hello via udp");
    assert_eq!(response, b"hello via udp");
    assert!(proxy.is_alive());
}

#[test]
fn tcptoudp_multiple_requests() {
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, echo.addr.port(), "tcptoudp"));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    for i in 0..3 {
        let msg = format!("request-{}", i);
        stream.write_all(msg.as_bytes()).unwrap();
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while got.len() < msg.len() {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed mid-response");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, msg.as_bytes());
    }
    assert!(proxy.is_alive());
}

#[test]
fn tcptoudp_server_never_responds() {
    // A UDP sink never answers: the handler's 100ms response-timeout arm must
    // fire repeatedly without breaking the client connection, and the sink
    // must receive the forwarded datagrams.
    let sink = spawn_udp_sink_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, sink.addr.port(), "tcptoudp"));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_millis(2500)))
        .unwrap();

    stream.write_all(b"fire and forget").unwrap();
    let mut buf = [0u8; 16];
    // No response is expected; the read times out (proxy kept looping).
    assert!(stream.read(&mut buf).is_err(), "expected no response");

    // Give the proxy a moment, then confirm the datagram was delivered.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        sink.received.load(Ordering::SeqCst) >= 1,
        "sink should have received the datagram"
    );
    assert!(proxy.is_alive());
}

#[test]
fn udp_to_tcp_forward() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, echo.addr.port(), "udptotcp"));
    let _ = proxy.bind_addr; // readiness checked via retries below

    let response = udp_round_trip_with_retries(proxy.bind_addr, b"ping over tcp");
    assert_eq!(response, b"ping over tcp");
    assert!(proxy.is_alive());
}

#[test]
fn udptotcp_persistent_stream() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, echo.addr.port(), "udptotcp"));

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // First datagram: triggers the TCP connection to the backend.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        socket.send_to(b"first", proxy.bind_addr).unwrap();
        let mut buf = [0u8; 64];
        if let Ok((n, _)) = socket.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"first");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no response to first datagram");
        std::thread::sleep(Duration::from_millis(100));
    }

    // Second datagram from the same UDP client must reuse the same TCP
    // connection to the backend.
    socket.send_to(b"second", proxy.bind_addr).unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = socket.recv_from(&mut buf).expect("response to second datagram");
    assert_eq!(&buf[..n], b"second");

    // Give the proxy a moment to process, then verify the backend saw exactly
    // one connection.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(echo.connections.load(Ordering::SeqCst), 1);
    assert!(proxy.is_alive());
}

#[test]
fn udptotcp_backend_down() {
    // No TCP backend: the proxy must log the failure, drop the datagram, and
    // keep serving (subsequent datagrams don't crash it).
    let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener);

    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&proxy_config(port, dead_port, "udptotcp"));

    // Retry a few datagrams: none can be answered.
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    for _ in 0..3 {
        socket.send_to(b"ping", proxy.bind_addr).unwrap();
        let _ = socket.recv_from(&mut [0u8; 64]);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(proxy.is_alive());
}
