mod common;
mod tls_common;

use common::*;
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

#[test]
fn tls_handshake_passthrough() {
    let env = tls_env();
    let server = spawn_tls_echo_server(&env);
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    // A full TLS handshake must succeed through the byte-transparent proxy,
    // and encrypted application data must round-trip.
    let response = tls_round_trip(&env, proxy.bind_addr, b"hello over tls");
    assert_eq!(response, b"hello over tls");
    assert!(proxy.is_alive());
}

#[test]
fn tls_bidirectional_data() {
    let env = tls_env();
    let server = spawn_tls_echo_server(&env);
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let mut stream = tls_connect(&env, proxy.bind_addr).expect("tls connect");
    use std::io::{Read, Write};

    for i in 0..5 {
        let msg = format!("message-{}", i);
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
fn tls_large_payload() {
    let env = tls_env();
    let server = spawn_tls_echo_server(&env);
    let port = reserve_proxy_port();
    let mut proxy = spawn_proxy(&tcp_proxy_config(port, server.addr.port()));
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let payload: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    let response = tls_round_trip(&env, proxy.bind_addr, &payload);
    assert_eq!(response.len(), payload.len());
    assert_eq!(response, payload);
    assert!(proxy.is_alive());
}
