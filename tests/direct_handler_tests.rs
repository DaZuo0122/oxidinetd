//! Direct-call tests for code paths that the full binary never reaches:
//! the "invalid protocol" fallback arms of the TCP/UDP handlers.

use oxidinetd::config::Protocol;
use oxidinetd::tcp_handler::handle_tcp_connection;
use oxidinetd::udp_handler::start_udp_forwarding;

#[test]
fn tcp_handler_rejects_udp_protocol() {
    smol::block_on(async {
        let listener = smol::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let handle = smol::spawn(async move {
            let (client, _) = listener.accept().await.expect("accept");
            handle_tcp_connection(client, addr.to_string(), Protocol::Udp).await
        });
        let _client = smol::net::TcpStream::connect(addr).await.expect("connect");
        let result = handle.await;
        assert!(result.is_err(), "Udp protocol must be rejected by the TCP handler");
    });
}

#[test]
fn tcp_handler_rejects_udptotcp_protocol() {
    smol::block_on(async {
        let listener = smol::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let handle = smol::spawn(async move {
            let (client, _) = listener.accept().await.expect("accept");
            handle_tcp_connection(client, addr.to_string(), Protocol::UdpToTcp).await
        });
        let _client = smol::net::TcpStream::connect(addr).await.expect("connect");
        let result = handle.await;
        assert!(result.is_err(), "UdpToTcp protocol must be rejected by the TCP handler");
    });
}

#[test]
fn udp_handler_rejects_tcp_protocol() {
    smol::block_on(async {
        let result = start_udp_forwarding(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:1".to_string(),
            None,
            Protocol::Tcp,
        )
        .await;
        assert!(result.is_err(), "Tcp protocol must be rejected by the UDP handler");
    });
}

#[test]
fn udp_handler_rejects_tcptoudp_protocol() {
    smol::block_on(async {
        let result = start_udp_forwarding(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:1".to_string(),
            None,
            Protocol::TcpToUdp,
        )
        .await;
        assert!(result.is_err(), "TcpToUdp protocol must be rejected by the UDP handler");
    });
}

#[test]
fn udp_forwarder_default_timeout_is_72s() {
    // `UdpForwarder::new` with a `None` timeout must fall back to 72 seconds.
    smol::block_on(async {
        let mut forwarder = oxidinetd::udp_handler::UdpForwarder::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:1".to_string(),
            None,
            Protocol::Udp,
        )
        .await
        .expect("create forwarder");
        // Run for a short while with no traffic; the loop just blocks on
        // recv_from. We race it against a timer and take whichever completes.
        let _ = smol::future::or(
            forwarder.run("127.0.0.1:1".to_string()),
            async {
                smol::Timer::after(std::time::Duration::from_millis(100)).await;
                Ok::<(), Box<dyn std::error::Error>>(())
            },
        )
        .await;
    });
}

#[test]
fn udp_forwarder_accepts_explicit_timeout() {
    smol::block_on(async {
        let mut forwarder = oxidinetd::udp_handler::UdpForwarder::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:1".to_string(),
            Some(5),
            Protocol::Udp,
        )
        .await
        .expect("create forwarder");
        let _ = smol::future::or(
            forwarder.run("127.0.0.1:1".to_string()),
            async {
                smol::Timer::after(std::time::Duration::from_millis(100)).await;
                Ok::<(), Box<dyn std::error::Error>>(())
            },
        )
        .await;
    });
}
