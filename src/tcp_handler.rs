use std::error::Error;
use std::net::SocketAddr;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

#[cfg(any(not(windows), feature = "tcp-profile"))]
async fn copy_tcp<A, B>(client: &mut A, backend: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    #[cfg(windows)]
    {
        let (client_read, client_write) = io::split(client);
        let (backend_read, backend_write) = io::split(backend);
        tokio::try_join!(
            copy_tcp_direction(client_read, backend_write),
            copy_tcp_direction(backend_read, client_write)
        )
    }
    #[cfg(not(windows))]
    io::copy_bidirectional(client, backend).await
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
async fn copy_tcp_streams(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> io::Result<(u64, u64)> {
    #[derive(Clone, Copy)]
    enum Mode {
        Adaptive,
        Fixed(usize),
        Pipeline,
    }
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    let mode = *MODE.get_or_init(|| match std::env::var("OI_COPY_BUFFER_MODE").as_deref() {
        Ok("fixed-8k") => Mode::Fixed(8 * 1024),
        Ok("fixed-16k") => Mode::Fixed(16 * 1024),
        Ok("fixed-32k") => Mode::Fixed(32 * 1024),
        Ok("pipeline-32k") => Mode::Pipeline,
        _ => Mode::Adaptive,
    });
    let common = socket_buffer_override("OI_COPY_SOCKET_BUFFER_KIB");
    let receive = socket_buffer_override("OI_COPY_RECV_BUFFER_KIB").or(common);
    let send = socket_buffer_override("OI_COPY_SEND_BUFFER_KIB").or(common);
    let client_receive = socket_buffer_override("OI_COPY_CLIENT_RECV_BUFFER_KIB").or(receive);
    let backend_receive = socket_buffer_override("OI_COPY_BACKEND_RECV_BUFFER_KIB").or(receive);
    if client_receive.is_some() || backend_receive.is_some() || send.is_some() {
        for (socket, receive) in [(&*client, client_receive), (&*backend, backend_receive)] {
            let socket = socket2::SockRef::from(socket);
            if let Some(size) = receive {
                socket.set_recv_buffer_size(size)?;
            }
            if let Some(size) = send {
                socket.set_send_buffer_size(size)?;
            }
        }
    }
    if std::env::var_os("OI_COPY_TRACE").is_some() {
        let client_socket = socket2::SockRef::from(&*client);
        let backend_socket = socket2::SockRef::from(&*backend);
        eprintln!(
            "OI_COPY_TRACE sockets client_rcvbuf={:?} client_sndbuf={:?} backend_rcvbuf={:?} backend_sndbuf={:?}",
            client_socket.recv_buffer_size(),
            client_socket.send_buffer_size(),
            backend_socket.recv_buffer_size(),
            backend_socket.send_buffer_size()
        );
    }
    if let Mode::Fixed(size) = mode {
        return io::copy_bidirectional_with_sizes(client, backend, size, size).await;
    }
    let (client_read, client_write) = client.split();
    let (backend_read, backend_write) = backend.split();
    if std::env::var_os("OI_COPY_TRACE").is_some() {
        return tokio::try_join!(
            copy_tcp_direction_traced(
                client_read,
                backend_write,
                matches!(mode, Mode::Pipeline),
                "client->backend"
            ),
            copy_tcp_direction_traced(
                backend_read,
                client_write,
                matches!(mode, Mode::Pipeline),
                "backend->client"
            )
        );
    }
    if let Mode::Pipeline = mode {
        return tokio::try_join!(
            copy_tcp_direction_pipeline(client_read, backend_write),
            copy_tcp_direction_pipeline(backend_read, client_write)
        );
    }
    tokio::try_join!(
        copy_tcp_direction(client_read, backend_write),
        copy_tcp_direction(backend_read, client_write)
    )
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
fn socket_buffer_override(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&kib| (64..=4096).contains(&kib))
        .map(|kib| kib * 1024)
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
struct MeasuredRead<R> {
    inner: R,
    polls: u64,
    pending: u64,
    chunks: u64,
    bytes: u64,
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
impl<R: AsyncRead + Unpin> AsyncRead for MeasuredRead<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.polls += 1;
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        match &result {
            std::task::Poll::Pending => self.pending += 1,
            std::task::Poll::Ready(Ok(())) => {
                let size = buf.filled().len() - before;
                if size != 0 {
                    self.chunks += 1;
                    self.bytes += size as u64;
                }
            }
            _ => {}
        }
        result
    }
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
struct MeasuredWrite<W> {
    inner: W,
    polls: u64,
    pending: u64,
    chunks: u64,
    bytes: u64,
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
impl<W: AsyncWrite + Unpin> AsyncWrite for MeasuredWrite<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.polls += 1;
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        match &result {
            std::task::Poll::Pending => self.pending += 1,
            std::task::Poll::Ready(Ok(size)) if *size != 0 => {
                self.chunks += 1;
                self.bytes += *size as u64;
            }
            _ => {}
        }
        result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
async fn copy_tcp_direction_traced<R, W>(
    reader: R,
    writer: W,
    pipeline: bool,
    direction: &str,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = MeasuredRead {
        inner: reader,
        polls: 0,
        pending: 0,
        chunks: 0,
        bytes: 0,
    };
    let mut writer = MeasuredWrite {
        inner: writer,
        polls: 0,
        pending: 0,
        chunks: 0,
        bytes: 0,
    };
    let result = if pipeline {
        copy_tcp_direction_pipeline(&mut reader, &mut writer).await
    } else {
        copy_tcp_direction(&mut reader, &mut writer).await
    };
    eprintln!(
        "OI_COPY_TRACE {direction} read_bytes={} read_chunks={} read_polls={} read_pending={} write_bytes={} write_chunks={} write_polls={} write_pending={}",
        reader.bytes,
        reader.chunks,
        reader.polls,
        reader.pending,
        writer.bytes,
        writer.chunks,
        writer.polls,
        writer.pending
    );
    result
}

#[cfg(all(windows, not(feature = "tcp-profile")))]
async fn copy_tcp_direction_pipeline<R, W>(mut reader: R, mut writer: W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    const BLOCK: usize = 32 * 1024;
    let mut first = vec![0; BLOCK];
    let mut second = vec![0; BLOCK];
    let mut size = reader.read(&mut first).await?;
    let mut send_first = true;
    let mut total = 0u64;
    while size != 0 {
        total += size as u64;
        size = if send_first {
            let (next, ()) =
                tokio::try_join!(reader.read(&mut second), writer.write_all(&first[..size]))?;
            next
        } else {
            let (next, ()) =
                tokio::try_join!(reader.read(&mut first), writer.write_all(&second[..size]))?;
            next
        };
        send_first = !send_first;
    }
    writer.shutdown().await?;
    Ok(total)
}

#[cfg(windows)]
async fn copy_tcp_direction<R, W>(mut reader: R, mut writer: W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use std::time::Duration;

    const MIN_SIZE: usize = 8 * 1024;
    const MAX_SIZE: usize = 32 * 1024;
    // Each direction scales independently. Idle and repeated small reads
    // release the larger buffer after the traffic mix changes.
    let mut buffer = vec![0; MIN_SIZE];
    let mut full_reads = 0u8;
    let mut small_reads = 0u8;
    let mut total = 0u64;
    loop {
        let read = if buffer.len() > MIN_SIZE {
            match tokio::time::timeout(Duration::from_millis(500), reader.read(&mut buffer)).await {
                Ok(result) => result,
                Err(_) => {
                    buffer = vec![0; buffer.len() / 2];
                    full_reads = 0;
                    small_reads = 0;
                    continue;
                }
            }
        } else {
            reader.read(&mut buffer).await
        };
        let n = match read {
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if n == 0 {
            writer.shutdown().await?;
            return Ok(total);
        }
        writer.write_all(&buffer[..n]).await?;
        total += n as u64;
        if n == buffer.len() {
            full_reads = full_reads.saturating_add(1);
            small_reads = 0;
            if full_reads >= 4 && buffer.len() < MAX_SIZE {
                buffer = vec![0; buffer.len() * 2];
                full_reads = 0;
            }
        } else if n <= buffer.len() / 4 {
            small_reads = small_reads.saturating_add(1);
            full_reads = 0;
            if small_reads >= 32 && buffer.len() > MIN_SIZE {
                buffer = vec![0; buffer.len() / 2];
                small_reads = 0;
            }
        } else {
            full_reads = 0;
            small_reads = 0;
        }
    }
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
            let mut server_stream = {
                #[cfg(windows)]
                {
                    static BLOCKING_POOL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                    if *BLOCKING_POOL.get_or_init(|| {
                        std::env::var("OI_BACKEND_CONNECT_MODE").as_deref() == Ok("blocking-pool")
                    }) {
                        let stream = tokio::task::spawn_blocking(move || {
                            std::net::TcpStream::connect(server_addr)
                        })
                        .await??;
                        stream.set_nonblocking(true)?;
                        TcpStream::from_std(stream)?
                    } else {
                        TcpStream::connect(&server_addr).await?
                    }
                }
                #[cfg(not(windows))]
                TcpStream::connect(&server_addr).await?
            };
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
            {
                #[cfg(windows)]
                copy_tcp_streams(&mut client_stream, &mut server_stream).await?;
                #[cfg(not(windows))]
                copy_tcp(&mut client_stream, &mut server_stream).await?;
            }
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
    #[cfg(windows)]
    if protocol == crate::config::Protocol::Tcp
        && std::env::var("OI_RELAY_BACKEND").as_deref() == Ok("rio")
    {
        return Ok(crate::rio_relay::start(bind_addr, connect_addr).await?);
    }
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
