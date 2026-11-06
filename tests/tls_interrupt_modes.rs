#![cfg(feature = "tcp-profile")]

mod tls_common;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tls_common::{TlsEnv, spawn_tls_echo_server, tls_connect, tls_env, tls_round_trip};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Async,
    BlockingPool,
}

impl Mode {
    const ALL: [Self; 2] = [Self::Async, Self::BlockingPool];
}

struct Proxy {
    child: Child,
    addr: SocketAddr,
    _config_dir: tempfile::TempDir,
}

impl Proxy {
    fn is_alive(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("inspect proxy process")
            .is_none()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn proxy(backend: SocketAddr, mode: Mode) -> Proxy {
    let bind = TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
    let addr = bind.local_addr().unwrap();
    drop(bind);
    let dir = tempfile::tempdir().expect("config directory");
    let path = dir.path().join("proxy.toml");
    std::fs::write(
        &path,
        format!(
            "[[forwarding_rules]]\nbind_address = \"127.0.0.1\"\nbind_port = {}\nconnect_address = \"127.0.0.1\"\nconnect_port = {}\nprotocol = \"tcp\"\n",
            addr.port(), backend.port()
        ),
    )
    .expect("write proxy config");
    let mut command = Command::new(env!("CARGO_BIN_EXE_oi"));
    command
        .arg("--config")
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("OI_PROFILE_SYNC_BACKEND_CONNECT")
        .env_remove("OI_PROFILE_BLOCKING_POOL_CONNECT");
    if matches!(mode, Mode::BlockingPool) {
        command.env("OI_PROFILE_BLOCKING_POOL_CONNECT", "1");
    }
    let child = command.spawn().expect("start proxy");
    let mut proxy = Proxy {
        child,
        addr,
        _config_dir: dir,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            return proxy;
        }
        assert!(proxy.is_alive(), "{mode:?} proxy exited before binding");
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{mode:?} proxy did not bind {addr}");
}

fn fault_server(config: Option<Arc<rustls::ServerConfig>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fault server");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(tcp) = accepted else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(3)));
                let _ = tcp.set_write_timeout(Some(Duration::from_secs(3)));
                if let Some(config) = config {
                    let conn = ServerConnection::new(config).expect("TLS server connection");
                    let mut stream = StreamOwned::new(conn, tcp);
                    let mut request = [0u8; 2];
                    if stream.read_exact(&mut request).is_ok() {
                        assert_eq!(&request, b"go");
                        stream
                            .write_all(&[0xAB; 4096])
                            .expect("partial TLS response");
                        stream.flush().expect("flush partial TLS response");
                    }
                    // No close_notify: the response stops halfway through.
                } else {
                    let mut hello = [0u8; 64];
                    let _ = tcp.peek(&mut hello);
                    // Close after receiving ClientHello bytes, without ServerHello.
                }
            });
        }
    });
    addr
}

#[test]
fn backend_aborts_tls_handshake_in_both_modes() {
    let env = tls_env();
    for mode in Mode::ALL {
        let backend = fault_server(None);
        let mut proxy = proxy(backend, mode);
        let mut client = tls_connect(&env, proxy.addr).expect("TCP connection to proxy");
        let mut byte = [0u8; 1];
        let result = client
            .write_all(b"hello")
            .and_then(|_| client.read(&mut byte).map(|_| ()));
        assert!(result.is_err(), "{mode:?}: truncated handshake succeeded");
        assert!(proxy.is_alive(), "{mode:?}: proxy died on handshake abort");
    }
}

#[test]
fn client_aborts_tls_handshake_in_both_modes() {
    let env = tls_env();
    let server = spawn_tls_echo_server(&env);
    for mode in Mode::ALL {
        let mut proxy = proxy(server.addr, mode);
        let conn = ClientConnection::new(
            env.client_config.clone(),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut hello = Vec::new();
        let mut conn = conn;
        conn.write_tls(&mut hello).expect("encode ClientHello");
        assert!(hello.len() > 8);
        let mut tcp = TcpStream::connect(proxy.addr).expect("connect to proxy");
        tcp.write_all(&hello[..8])
            .expect("send partial ClientHello");
        drop(tcp);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(tls_round_trip(&env, proxy.addr, b"recovered"), b"recovered");
        assert!(proxy.is_alive(), "{mode:?}: proxy died on client abort");
    }
}

#[test]
fn backend_aborts_tls_transfer_in_both_modes() {
    let env = tls_env();
    for mode in Mode::ALL {
        let backend = fault_server(Some(env.server_config.clone()));
        let mut proxy = proxy(backend, mode);
        let mut client = tls_connect(&env, proxy.addr).expect("connect to proxy");
        client.write_all(b"go").expect("send request");
        let mut incomplete = [0u8; 8192];
        assert!(
            client.read_exact(&mut incomplete).is_err(),
            "{mode:?}: truncated TLS response succeeded"
        );
        assert!(incomplete[..4096].iter().all(|&byte| byte == 0xAB));
        drop(client);
        let mut next = tls_connect(&env, proxy.addr).expect("reconnect to proxy");
        next.write_all(b"go").expect("send second request");
        let mut prefix = [0u8; 4096];
        next.read_exact(&mut prefix)
            .expect("read second response prefix");
        assert!(prefix.iter().all(|&byte| byte == 0xAB));
        assert!(proxy.is_alive(), "{mode:?}: proxy died on transfer abort");
    }
}

#[test]
fn client_aborts_tls_transfer_in_both_modes() {
    let env: TlsEnv = tls_env();
    let server = spawn_tls_echo_server(&env);
    for mode in Mode::ALL {
        let mut proxy = proxy(server.addr, mode);
        let mut client = tls_connect(&env, proxy.addr).expect("connect to proxy");
        client
            .write_all(&[0xCD; 64 * 1024])
            .expect("write TLS payload");
        drop(client); // No close_notify while the echo is still in flight.
        assert_eq!(tls_round_trip(&env, proxy.addr, b"recovered"), b"recovered");
        assert!(
            proxy.is_alive(),
            "{mode:?}: proxy died on client transfer abort"
        );
    }
}
