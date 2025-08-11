use smol::net::{UdpSocket, TcpStream};
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::error::Error;

pub struct UdpForwarder {
    socket: UdpSocket,
    connections: HashMap<SocketAddr, UdpConnection>,
    timeout: Duration,
    protocol: crate::config::Protocol,
}

pub struct UdpConnection {
    remote_addr: SocketAddr,
    last_activity: Instant,
    tcp_stream: Option<TcpStream>,
    buffer: Vec<u8>,
}

impl UdpForwarder {
    pub async fn new(bind_addr: SocketAddr, _connect_addr: String, timeout: Option<u64>, protocol: crate::config::Protocol) -> Result<Self, Box<dyn std::error::Error>> {
        let socket = UdpSocket::bind(bind_addr).await?;
        
        let timeout_duration = timeout
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(72)); // Default UDP timeout
        
        Ok(UdpForwarder {
            socket,
            connections: HashMap::new(),
            timeout: timeout_duration,
            protocol,
        })
    }
    
    pub async fn run(&mut self, connect_addr: String) -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = vec![0; 65536];
        
        match self.protocol {
            crate::config::Protocol::Udp => {
                loop {
                    // Clean up expired connections
                    let now = Instant::now();
                    self.connections.retain(|_, conn| {
                        now.duration_since(conn.last_activity) < self.timeout
                    });
                    
                    let (len, src_addr) = self.socket.recv_from(&mut buf).await?;
                    
                    // Create a new socket for each destination to maintain source IP
                    let server_socket = UdpSocket::bind("0.0.0.0:0").await?;
                    server_socket.connect(&connect_addr).await?;
                    
                    // Forward data to connected server
                    server_socket.send(&buf[..len]).await?;
                    
                    // Update connection tracking
                    self.connections.insert(
                        src_addr,
                        UdpConnection {
                            remote_addr: src_addr,
                            last_activity: Instant::now(),
                            tcp_stream: None,
                            buffer: buf[..len].to_vec(),
                        }
                    );
                    
                    // Try to receive response from server
                    match smol::future::or(
                        async {
                            let response_len = server_socket.recv(&mut buf).await?;
                            self.socket.send_to(&buf[..response_len], src_addr).await?;
                            Ok::<(), Box<dyn std::error::Error>>(())
                        },
                        async {
                            // Timeout after 1 second if no response
                            smol::Timer::after(Duration::from_secs(1)).await;
                            Ok::<(), Box<dyn std::error::Error>>(())
                        }
                    ).await {
                        Ok(_) => {},
                        Err(e) => {
                            eprintln!("UDP response error: {}", e);
                        }
                    }
                }
            },
            crate::config::Protocol::UdpToTcp => {
                loop {
                    let (len, src_addr) = self.socket.recv_from(&mut buf).await?;
                    
                    // Get or create connection for this client
                    let connection = self.connections.entry(src_addr).or_insert_with(|| {
                        UdpConnection {
                            remote_addr: src_addr,
                            last_activity: Instant::now(),
                            tcp_stream: None,
                            buffer: Vec::new(),
                        }
                    });
                    
                    connection.last_activity = Instant::now();
                    
                    // Connect to TCP server if not already connected
                    if connection.tcp_stream.is_none() {
                        match TcpStream::connect(&connect_addr).await {
                            Ok(stream) => {
                                connection.tcp_stream = Some(stream);
                            },
                            Err(e) => {
                                eprintln!("Failed to connect to TCP server: {}", e);
                                continue;
                            }
                        }
                    }
                    
                    // Forward data to TCP server
                    if let Some(ref mut tcp_stream) = connection.tcp_stream {
                        if let Err(e) = tcp_stream.write_all(&buf[..len]).await {
                            eprintln!("Failed to write to TCP stream: {}", e);
                            connection.tcp_stream = None; // Mark connection as broken
                            continue;
                        }
                        
                        // Try to read response from TCP server with timeout
                        let result = smol::future::or(
                            async {
                                let mut response_buf = vec![0; 65536];
                                match tcp_stream.peek(&mut response_buf).await {
                                    Ok(0) => {
                                        // Connection closed
                                        Ok::<Vec<u8>, Box<dyn Error + Send + Sync>>(Vec::new())
                                    },
                                    Ok(n) => {
                                        // Read the data we just peeked at
                                        let _ = tcp_stream.read(&mut response_buf[..n]).await?;
                                        Ok::<Vec<u8>, Box<dyn Error + Send + Sync>>(response_buf[..n].to_vec())
                                    },
                                    Err(e) => Err(Box::new(e) as Box<dyn Error + Send + Sync>),
                                }
                            },
                            async {
                                smol::Timer::after(Duration::from_millis(100)).await;
                                Ok::<Vec<u8>, Box<dyn Error + Send + Sync>>(Vec::new())
                            }
                        ).await;
                        
                        match result {
                            Ok(data) if !data.is_empty() => {
                                // Forward response to UDP client
                                if let Err(e) = self.socket.send_to(&data, src_addr).await {
                                    eprintln!("Failed to send response to UDP client: {}", e);
                                }
                            },
                            Ok(_) => {}, // Timeout, no data
                            Err(e) => {
                                eprintln!("Error reading from TCP stream: {}", e);
                                connection.tcp_stream = None; // Mark connection as broken
                            },
                        }
                    }
                }
            },
            _ => return Err("Invalid protocol for UDP handler".into()),
        }
    }
}

pub async fn start_udp_forwarding(
    bind_addr: std::net::SocketAddr,
    connect_addr: String,
    timeout: Option<u64>,
    protocol: crate::config::Protocol,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut forwarder = UdpForwarder::new(bind_addr, connect_addr.clone(), timeout, protocol).await?;
    forwarder.run(connect_addr).await
}