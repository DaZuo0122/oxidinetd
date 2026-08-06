mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

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

#[test]
fn tcp_basic_forward() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"hello world");
    assert_eq!(response, b"hello world");

    assert!(proxy.is_alive());
}

#[test]
fn tcp_large_payload() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let payload: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    let response = tcp_round_trip(proxy.bind_addr, &payload);
    assert_eq!(response.len(), payload.len());
    assert_eq!(response, payload);

    assert!(proxy.is_alive());
}

#[test]
fn tcp_multiple_connections() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut handles = Vec::new();
    for i in 0..10 {
        let proxy_addr = proxy.bind_addr;
        handles.push(std::thread::spawn(move || {
            let payload = format!("client-{}", i);
            let response = tcp_round_trip(proxy_addr, payload.as_bytes());
            response == payload.as_bytes()
        }));
    }
    for handle in handles {
        assert!(handle.join().unwrap());
    }
    assert!(proxy.is_alive());
}

#[test]
fn tcp_client_disconnect() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, echo.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    {
        let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
        stream.write_all(b"some data").unwrap();
        // Drop the connection abruptly without reading the response.
    }
    std::thread::sleep(Duration::from_millis(300));

    // The proxy must still be alive and able to serve new connections.
    let response = tcp_round_trip(proxy.bind_addr, b"still alive");
    assert_eq!(response, b"still alive");
    assert!(proxy.is_alive());
}

#[test]
fn tcp_server_unreachable() {
    // Point the proxy at a port with no listener.
    let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener);

    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, dead_port));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = TcpStream::connect(proxy.bind_addr).expect("connect to proxy");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        // The proxy cannot reach the backend, so the client sees EOF or a reset.
        Ok(0) => {}
        Ok(n) => panic!("expected EOF, got {} bytes", n),
        Err(_) => {}
    }

    assert!(proxy.is_alive());
}

#[test]
fn tcp_bind_port_in_use() {
    let echo = spawn_tcp_echo_server();

    // Occupy a port with a listener of our own; retry with fresh random
    // ports until one is actually free to occupy.
    let (occupied, port) = loop {
        let port = reserve_proxy_port();
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => break (listener, port),
            Err(_) => continue,
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, tcp_proxy_config(port, echo.addr.port())).unwrap();

    let mut child = std::process::Command::new(BIN)
        .arg("-c")
        .arg(&path)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn oi binary");

    // Give the proxy time to attempt (and fail) the bind.
    std::thread::sleep(Duration::from_millis(1500));

    // The process must not have crashed: it reports the bind error and keeps
    // waiting for shutdown.
    let status = child.try_wait().expect("try_wait");
    assert!(status.is_none(), "proxy exited unexpectedly");

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TCP forwarding error"),
        "expected bind error on stderr, got: {}",
        stderr
    );

    drop(occupied);
}
