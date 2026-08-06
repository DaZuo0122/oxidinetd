//! Shared TLS test infrastructure. Only a subset of the servers is used by
//! each test binary, so dead-code analysis is disabled for the whole module.
#![allow(dead_code)]

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub struct TlsEnv {
    pub client_config: Arc<ClientConfig>,
    pub server_config: Arc<ServerConfig>,
}

pub fn tls_env() -> TlsEnv {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate cert");
    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

    let mut root_store = RootCertStore::empty();
    root_store.add(cert_der.clone()).expect("add root cert");

    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server cert");

    TlsEnv {
        client_config: Arc::new(client_config),
        server_config: Arc::new(server_config),
    }
}

pub struct TlsEchoServer {
    pub addr: SocketAddr,
    handle: std::thread::JoinHandle<()>,
}

/// TLS server that echoes everything it receives.
pub fn spawn_tls_echo_server(env: &TlsEnv) -> TlsEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls server");
    let addr = listener.local_addr().unwrap();
    let config = env.server_config.clone();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let conn = ServerConnection::new(config).expect("server conn");
                let mut stream = StreamOwned::new(conn, stream);
                let mut buf = vec![0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    TlsEchoServer { addr, handle }
}

pub struct TlsEchoOnceServer {
    pub addr: SocketAddr,
    handle: std::thread::JoinHandle<()>,
}

/// TLS server that echoes the first message, then abruptly drops the
/// connection.
pub fn spawn_tls_echo_once_server(env: &TlsEnv) -> TlsEchoOnceServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls server");
    let addr = listener.local_addr().unwrap();
    let config = env.server_config.clone();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                let conn = ServerConnection::new(config).expect("server conn");
                let mut stream = StreamOwned::new(conn, stream);
                let mut buf = [0u8; 16384];
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let _ = stream.write_all(&buf[..n]);
                    }
                }
                // drop: connection torn down right after the first echo
            });
        }
    });
    TlsEchoOnceServer { addr, handle }
}

pub struct TlsSlowHandshakeServer {
    pub addr: SocketAddr,
    handle: std::thread::JoinHandle<()>,
}

/// TLS server that sleeps before performing the handshake, simulating a slow
/// TLS endpoint. TLS 1.3 handshake starts lazily on the first read/write.
pub fn spawn_tls_slow_handshake_server(env: &TlsEnv, delay: Duration) -> TlsSlowHandshakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls server");
    let addr = listener.local_addr().unwrap();
    let config = env.server_config.clone();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            std::thread::spawn(move || {
                // Handshake bytes arrive but the server stalls.
                std::thread::sleep(delay);
                let conn = ServerConnection::new(config).expect("server conn");
                let mut stream = StreamOwned::new(conn, stream);
                let mut buf = vec![0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    TlsSlowHandshakeServer { addr, handle }
}

pub fn tls_connect(
    env: &TlsEnv,
    addr: SocketAddr,
) -> std::io::Result<StreamOwned<ClientConnection, TcpStream>> {
    let tcp = TcpStream::connect(addr)?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    let conn = ClientConnection::new(
        env.client_config.clone(),
        ServerName::try_from("localhost").expect("valid server name"),
    )
    .expect("client conn");    Ok(StreamOwned::new(conn, tcp))
}

pub fn tls_round_trip(env: &TlsEnv, addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut stream = tls_connect(env, addr).expect("tls connect");
    // The proxy does not propagate a TLS close_notify as a TCP FIN, so the
    // server never closes on its own. Write in chunks and read the echoed
    // bytes interleaved to avoid stalling on saturated socket buffers.
    let mut response = Vec::with_capacity(payload.len());
    let mut buf = [0u8; 32768];
    let mut expected = 0;
    for chunk in payload.chunks(262_144) {
        stream.write_all(chunk).expect("tls write");
        expected += chunk.len();
        while response.len() < expected {
            let n = stream.read(&mut buf).expect("tls read");
            assert!(n > 0, "connection closed before the full response arrived");
            response.extend_from_slice(&buf[..n]);
        }
    }
    response
}
