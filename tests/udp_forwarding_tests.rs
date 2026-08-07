mod common;

use common::*;
use std::time::Duration;

fn udp_proxy_config(bind_port: u16, connect_port: u16, timeout: Option<u64>) -> String {
    let timeout_line = match timeout {
        Some(secs) => format!("timeout = {}\n", secs),
        None => String::new(),
    };
    format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "udp"
{}
"#,
        bind_port, connect_port, timeout_line
    )
}

#[test]
fn udp_basic_forward() {
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, echo.addr.port(), None));

    let response = udp_round_trip_with_retries(proxy.bind_addr, b"ping");
    assert_eq!(response, b"ping");
    assert!(proxy.is_alive());
}

#[test]
fn udp_multiple_clients() {
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, echo.addr.port(), None));

    let mut handles = Vec::new();
    for i in 0..5 {
        let proxy_addr = proxy.bind_addr;
        handles.push(std::thread::spawn(move || {
            let payload = format!("client-{}", i);
            for _ in 0..3 {
                // Retry: the proxy's UDP socket may not be bound yet.
                let response = udp_round_trip_with_retries(proxy_addr, payload.as_bytes());
                if response != payload.as_bytes() {
                    return false;
                }
            }
            true
        }));
    }
    for handle in handles {
        assert!(handle.join().unwrap());
    }
    assert!(proxy.is_alive());
}

#[test]
fn udp_timeout_expiry() {
    // A 1-second timeout: after the client goes idle for longer than that,
    // the proxy prunes the tracked connection and keeps serving.
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, echo.addr.port(), Some(1)));

    let response = udp_round_trip_with_retries(proxy.bind_addr, b"first");
    assert_eq!(response, b"first");

    // Outlive the 1s timeout while the proxy is idle.
    std::thread::sleep(Duration::from_millis(2200));

    let response = udp_round_trip_with_retries(proxy.bind_addr, b"second");
    assert_eq!(response, b"second");
    assert!(proxy.is_alive());
}

#[test]
fn udp_large_datagram() {
    let echo = spawn_udp_echo_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, echo.addr.port(), None));

    // Close to the 65507-byte maximum UDP payload.
    let payload: Vec<u8> = (0..65000).map(|i| (i % 251) as u8).collect();
    let response = udp_round_trip_with_retries(proxy.bind_addr, &payload);
    assert_eq!(response.len(), payload.len());
    assert_eq!(response, payload);
    assert!(proxy.is_alive());
}

#[test]
fn udp_no_response_timeout() {
    // The backend never responds: the proxy's 1s response timeout must fire
    // without hanging, and the proxy must keep serving subsequent datagrams.
    let sink = spawn_udp_sink_server();
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&udp_proxy_config(port, sink.addr.port(), None));

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // UDP is connectionless: datagrams sent before the proxy's socket is
    // bound are silently lost. Re-send until the sink confirms delivery.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sink.received.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "proxy never forwarded the datagram");
        socket.send_to(b"no reply", proxy.bind_addr).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }

    let started = std::time::Instant::now();
    socket.send_to(b"no reply", proxy.bind_addr).unwrap();
    let result = socket.recv_from(&mut vec![0u8; 64]);
    // The client must NOT receive anything back...
    assert!(result.is_err(), "expected no response from sink");
    // ...and the client-side wait must terminate well within the read timeout
    // (the proxy kept running and fired its own 1s response timeout).
    assert!(started.elapsed() < Duration::from_millis(2500));

    // The sink must have received the datagrams.
    assert!(sink.received.load(std::sync::atomic::Ordering::SeqCst) >= 1);

    // The proxy must still be alive and forwarding (the backend still never
    // responds, so delivery is verified through the sink instead).
    let before = sink.received.load(std::sync::atomic::Ordering::SeqCst);
    socket.send_to(b"after timeout", proxy.bind_addr).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sink.received.load(std::sync::atomic::Ordering::SeqCst) == before {
        assert!(
            std::time::Instant::now() < deadline,
            "proxy stopped forwarding after the timeout fired"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(proxy.is_alive());
}

#[test]
fn udp_bind_port_in_use() {
    let echo = spawn_udp_echo_server();

    // Occupy a port with a UDP socket of our own; retry with fresh random
    // ports until one is actually free to occupy.
    let (occupied, port) = loop {
        let port = reserve_proxy_port();
        match std::net::UdpSocket::bind(("127.0.0.1", port)) {
            Ok(socket) => break (socket, port),
            Err(_) => continue,
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, udp_proxy_config(port, echo.addr.port(), None)).unwrap();

    let mut child = std::process::Command::new(BIN)
        .arg("-c")
        .arg(&path)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn oi binary");

    std::thread::sleep(Duration::from_millis(1500));

    let status = child.try_wait().expect("try_wait");
    assert!(status.is_none(), "proxy exited unexpectedly");

    terminate_proxy(&mut child);
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("UDP forwarding error"),
        "expected bind error on stderr, got: {}",
        stderr
    );

    drop(occupied);
}
