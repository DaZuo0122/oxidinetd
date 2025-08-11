use smol::net::{TcpListener, TcpStream, UdpSocket};
use smol::io;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::net::SocketAddr;
use std::error::Error;

pub async fn handle_tcp_connection(
    mut client_stream: TcpStream,
    server_addr: String,
    protocol: crate::config::Protocol,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match protocol {
        crate::config::Protocol::Tcp => {
            let server_stream = TcpStream::connect(&server_addr).await?;
            
            // Use smol's copy function to forward data in both directions
            let client_to_server = io::copy(client_stream.clone(), server_stream.clone());
            let server_to_client = io::copy(server_stream, client_stream);
            
            futures_lite::future::try_zip(client_to_server, server_to_client).await?;
        },
        crate::config::Protocol::TcpToUdp => {
            // Create a UDP socket for forwarding
            let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
            let server_socket_addr: SocketAddr = server_addr.parse()?;
            udp_socket.connect(&server_socket_addr).await?;
            
            // Buffer for data transfer
            let mut tcp_buffer = vec![0; 65536];
            let mut udp_buffer = vec![0; 65536];
            
            loop {
                // Try to read from TCP client
                match client_stream.peek(&mut tcp_buffer).await {
                    Ok(0) => break, // Connection closed
                    Ok(n) => {
                        // Forward data to UDP server
                        udp_socket.send(&tcp_buffer[..n]).await?;
                        // Consume the data we just peeked at
                        let _ = client_stream.read(&mut tcp_buffer[..n]).await?;
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No data available right now, continue
                    },
                    Err(e) => return Err(Box::new(e)),
                }
                
                // Try to read from UDP server with a timeout
                let result = smol::future::or(
                    async {
                        let len = udp_socket.recv(&mut udp_buffer).await?;
                        Ok::<usize, Box<dyn Error + Send + Sync>>(len)
                    },
                    async {
                        smol::Timer::after(std::time::Duration::from_millis(100)).await;
                        Ok::<usize, Box<dyn Error + Send + Sync>>(0)
                    }
                ).await;
                
                match result {
                    Ok(len) if len > 0 => {
                        // Forward data to TCP client
                        client_stream.write_all(&udp_buffer[..len]).await?;
                    },
                    Ok(_) => {}, // Timeout, no data
                    Err(e) => return Err(e),
                }
            }
        },
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
        println!("New connection from {}", client_addr);
        
        // Clone the connect_addr for each connection
        let connect_addr_clone = connect_addr.clone();
        let protocol_clone = protocol.clone();
        
        // Spawn a new task to handle this connection
        smol::spawn(async move {
            if let Err(e) = handle_tcp_connection(client_stream, connect_addr_clone, protocol_clone).await {
                eprintln!("Connection error: {}", e);
            }
        }).detach();
    }
}