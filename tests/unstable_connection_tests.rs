mod common;
mod tls_common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use tls_common::*;

fn tcp_proxy_config(bind_port: u16, connect_port: u16) -> String {
    format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "tcp"
"#,
        bind_port, connect_port
    )
}

fn udp_proxy_config(bind_port: u16, connect_port: u16) -> String {
    format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "udp"
"#,
        bind_port, connect_port
    )
}

#[test]
fn tcp_client_drop_mid_transfer() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    {
        let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
        let payload = vec![0xABu8; 65536];
        stream.write_all(&payload).unwrap();
        // The echo comes back while the client still holds unread data; the
        // abrupt drop then forces an RST towards the proxy.
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(300));

    // The proxy must survive the drop and serve new connections.
    let response = tcp_round_trip(proxy.bind_addr, b"after client drop");
    assert_eq!(response, b"after client drop");
    assert!(proxy.is_alive());
}

#[test]
fn tcp_server_drop_mid_transfer() {
    // The backend reads one chunk then drops with unread data pending (RST).
    let server = spawn_tcp_close_after_read_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(&[0x42u8; 8192]).unwrap();
    let mut buf = [0u8; 256];
    let read = stream.read(&mut buf);
    // The client observes a reset or EOF; either way the connection is gone.
    assert!(
        read.is_err() || matches!(read, Ok(0)),
        "expected reset or EOF, got {:?}",
        read
    );

    // The proxy must still serve new connections.
    let echo2 = spawn_tcp_echo_server();
    let port2 = reserve_proxy_port();
    let mut proxy2 = spawn_proxy(&tcp_proxy_config(port2, echo2.addr.port()));
    assert!(wait_for_port(proxy2.bind_addr, Duration::from_secs(10)));
    assert_eq!(tcp_round_trip(proxy2.bind_addr, b"recovered"), b"recovered");
    assert!(proxy.is_alive());
    assert!(proxy2.is_alive());
}

#[test]
fn tls_client_drop_mid_handshake() {
    // The "TLS server" is a plain listener that accepts and immediately
    // closes: the handshake cannot complete, so the client sees a failure.
    let server = spawn_tcp_close_immediately_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let env = tls_env();
    let result = tls_connect(&env, proxy.bind_addr)
        .and_then(|mut stream| {
            let mut buf = [0u8; 64];
            stream.write_all(b"ClientHello").map(|_| stream.read(&mut buf))
        });
    assert!(result.is_err(), "expected TLS handshake failure");

    // The proxy must survive the aborted handshake and forward plain traffic.
    let echo = spawn_tcp_echo_server();
    let port2 = reserve_proxy_port();
    let mut proxy2 = spawn_proxy(&tcp_proxy_config(port2, echo.addr.port()));
    assert!(wait_for_port(proxy2.bind_addr, Duration::from_secs(10)));
    assert_eq!(tcp_round_trip(proxy2.bind_addr, b"plain works"), b"plain works");
    assert!(proxy.is_alive());
    assert!(proxy2.is_alive());
}

#[test]
fn tls_server_drop_post_handshake() {
    let env = tls_env();
    let server = spawn_tls_echo_once_server(&env);
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = tls_connect(&env, proxy.bind_addr).expect("tls connect");
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).expect("read first echo");
    assert_eq!(&buf[..n], b"ping");

    // The server tore the connection down right after the echo: the next
    // read must terminate (EOF or error) instead of hanging.
    let n = stream.read(&mut buf);
    assert!(
        n.is_err() || matches!(n, Ok(0)),
        "expected EOF/error after server drop, got {:?}",
        n
    );

    // A brand new TLS session through the same proxy must work.
    let response = tls_round_trip(&env, proxy.bind_addr, b"fresh session");
    assert_eq!(response, b"fresh session");
    assert!(proxy.is_alive());
}

#[test]
fn tcp_slow_client() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // Write the message in tiny chunks with delays.
    for chunk in ["hello", " ", "slow", " ", "client"] {
        stream.write_all(chunk.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }

    // Read the exact echo (the proxy does not propagate half-close, so the
    // connection never reaches EOF on its own).
    let expected = "hello slow client".as_bytes();
    let mut response = Vec::with_capacity(expected.len());
    let mut buf = [0u8; 64];
    while response.len() < expected.len() {
        let n = stream.read(&mut buf).expect("read echo");
        assert!(n > 0, "connection closed before full echo received");
        response.extend_from_slice(&buf[..n]);
    }
    assert_eq!(response, expected);
    assert!(proxy.is_alive());
}

#[test]
fn tcp_slow_server() {
    let echo = spawn_tcp_delayed_echo_server(Duration::from_millis(500));
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"slow response");
    assert_eq!(response, b"slow response");
    assert!(proxy.is_alive());
}

#[test]
fn tls_slow_handshake() {
    let env = tls_env();
    let server = spawn_tls_slow_handshake_server(&env, Duration::from_millis(1000));
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    // The handshake stalls for a second; the proxy must not interfere.
    let response = tls_round_trip(&env, proxy.bind_addr, b"slow handshake");
    assert_eq!(response, b"slow handshake");
    assert!(proxy.is_alive());
}

#[test]
fn udp_delayed_response() {
    // 400ms delay is inside the proxy's 1s response window.
    let echo = spawn_udp_delayed_echo_server(Duration::from_millis(400));
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, echo.addr.port()));

    let response = udp_round_trip_with_retries(proxy.bind_addr, b"slow udp");
    assert_eq!(response, b"slow udp");
    assert!(proxy.is_alive());
}

#[test]
fn tcp_fast_client_slow_server() {
    // The backend reads slowly: the client-to-server path must apply
    // backpressure without losing or corrupting data.
    let echo = spawn_tcp_slow_read_echo_server(16 * 1024, Duration::from_millis(5));
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let payload: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let received = bidirectional_echo(proxy.bind_addr, &payload);
    assert_eq!(received, payload);
    assert!(proxy.is_alive());
}

#[test]
fn tcp_backpressure_bidirectional() {
    // The backend echoes slowly (server-to-client throttled); the client
    // writes fast while reading. Both directions must complete uncorrupted.
    let echo = spawn_tcp_slow_write_echo_server(16 * 1024, Duration::from_millis(5));
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let received = bidirectional_echo(proxy.bind_addr, &payload);
    assert_eq!(received, payload);
    assert!(proxy.is_alive());
}

#[test]
fn tls_backpressure() {
    let env = tls_env();
    let server = spawn_tls_echo_server(&env);
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    // Stream a large payload through the TLS session in both directions and
    // verify the encrypted data is not corrupted. Writes and reads are
    // interleaved so neither side outruns the other.
    let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    let mut stream = tls_connect(&env, proxy.bind_addr).expect("tls connect");

    let mut received = Vec::with_capacity(payload.len());
    let mut buf = [0u8; 32768];
    for chunk in payload.chunks(262_144) {
        stream.write_all(chunk).expect("write tls chunk");
        let mut got = 0;
        while got < chunk.len() {
            let n = stream.read(&mut buf).expect("read tls chunk");
            assert!(n > 0, "connection closed mid-payload");
            got += n;
            received.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(received, payload);
    assert!(proxy.is_alive());
}

/// Writes `payload` to the proxy from a background thread while the calling
/// thread reads the echo, then returns everything received.
fn bidirectional_echo(proxy_addr: std::net::SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let mut write_stream = stream.try_clone().unwrap();
    let payload_owned = payload.to_vec();
    let writer = std::thread::spawn(move || {
        write_stream.write_all(&payload_owned).unwrap();
    });

    let mut received = Vec::new();
    let mut buf = [0u8; 65536];
    while received.len() < payload.len() {
        let n = stream.read(&mut buf).expect("read echo");
        assert!(n > 0, "connection closed before full echo received");
        received.extend_from_slice(&buf[..n]);
    }
    writer.join().unwrap();
    received
}
