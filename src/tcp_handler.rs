use std::error::Error;
use std::net::SocketAddr;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

async fn copy_tcp<A, B>(client: &mut A, backend: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    #[cfg(windows)]
    {
        // Response-heavy traffic benefits from a larger backend-to-client
        // buffer. Keep the request direction small to limit per-flow memory.
        const CLIENT_TO_BACKEND_BUFFER: usize = 8 * 1024;
        const BACKEND_TO_CLIENT_BUFFER: usize = 32 * 1024;
        io::copy_bidirectional_with_sizes(
            client,
            backend,
            CLIENT_TO_BACKEND_BUFFER,
            BACKEND_TO_CLIENT_BUFFER,
        )
        .await
    }
    #[cfg(not(windows))]
    io::copy_bidirectional(client, backend).await
}

pub async fn handle_tcp_connection(
    mut client_stream: TcpStream,
    server_addr: String,
    protocol: crate::config::Protocol,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match protocol {
        crate::config::Protocol::Tcp => {
            #[cfg(feature = "tcp-profile")]
            let profile_addr = client_stream.peer_addr()?;
            #[cfg(feature = "tcp-profile")]
            crate::tcp_profile::emit(
                profile_addr,
                crate::tcp_profile::Stage::BackendConnectStarted,
            );
            #[cfg(not(feature = "tcp-profile"))]
            let mut server_stream = TcpStream::connect(&server_addr).await?;
            #[cfg(feature = "tcp-profile")]
            let mut server_stream =
                if std::env::var_os("OI_PROFILE_BLOCKING_POOL_CONNECT").is_some() {
                    let address = server_addr.clone();
                    let stream =
                        tokio::task::spawn_blocking(move || std::net::TcpStream::connect(address))
                            .await??;
                    stream.set_nonblocking(true)?;
                    TcpStream::from_std(stream)?
                } else if std::env::var_os("OI_PROFILE_SYNC_BACKEND_CONNECT").is_some() {
                    let stream = std::net::TcpStream::connect(&server_addr)?;
                    stream.set_nonblocking(true)?;
                    TcpStream::from_std(stream)?
                } else {
                    TcpStream::connect(&server_addr).await?
                };
            #[cfg(feature = "tcp-profile")]
            crate::tcp_profile::emit(profile_addr, crate::tcp_profile::Stage::BackendConnected);

            #[cfg(not(feature = "tcp-profile"))]
            copy_tcp(&mut client_stream, &mut server_stream).await?;
            #[cfg(feature = "tcp-profile")]
            {
                use crate::tcp_profile::{ProfiledStream, Stage};
                let mut client = ProfiledStream::new(
                    &mut client_stream,
                    profile_addr,
                    Stage::ClientRead,
                    Stage::ClientWritten,
                );
                let mut backend = ProfiledStream::new(
                    &mut server_stream,
                    profile_addr,
                    Stage::BackendRead,
                    Stage::BackendWritten,
                );
                crate::tcp_profile::emit(profile_addr, Stage::CopyStarted);
                copy_tcp(&mut client, &mut backend).await?;
            }
        }
        crate::config::Protocol::TcpToUdp => {
            // Create a UDP socket for forwarding
            let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            let server_socket_addr: SocketAddr = server_addr.parse()?;
            udp_socket.connect(server_socket_addr).await?;

            // Buffer for data transfer
            let mut tcp_buffer = vec![0; 65536];
            let mut udp_buffer = vec![0; 65536];

            loop {
                // Read each TCP chunk once before forwarding it as a UDP datagram.
                match client_stream.read(&mut tcp_buffer).await {
                    Ok(0) => break, // Connection closed
                    Ok(n) => {
                        // Forward data to UDP server
                        udp_socket.send(&tcp_buffer[..n]).await?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No data available right now, continue
                    }
                    Err(e) => return Err(Box::new(e)),
                }

                // Try to read from UDP server with a timeout
                let result = tokio::select! {
                    result = udp_socket.recv(&mut udp_buffer) => result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => Ok(0),
                };

                match result {
                    Ok(len) if len > 0 => {
                        // Forward data to TCP client
                        client_stream.write_all(&udp_buffer[..len]).await?;
                    }
                    Ok(_) => {} // Timeout, no data
                    Err(e) => return Err(e),
                }
            }
        }
        _ => return Err("Invalid protocol for TCP handler".into()),
    }

    Ok(())
}

pub async fn start_tcp_forwarding(
    bind_addr: std::net::SocketAddr,
    connect_addr: String,
    protocol: crate::config::Protocol,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(bind_addr).await?;

    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        #[cfg(feature = "tcp-profile")]
        crate::tcp_profile::emit(client_addr, crate::tcp_profile::Stage::Accepted);
        println!("New connection from {}", client_addr);

        // Clone the connect_addr for each connection
        let connect_addr_clone = connect_addr.clone();
        let protocol_clone = protocol.clone();

        // Spawn a new task to handle this connection
        tokio::spawn(async move {
            #[cfg(feature = "tcp-profile")]
            crate::tcp_profile::emit(client_addr, crate::tcp_profile::Stage::HandlerStarted);
            if let Err(e) =
                handle_tcp_connection(client_stream, connect_addr_clone, protocol_clone).await
            {
                eprintln!("Connection error: {}", e);
            }
        });
    }
}
