//! Diagnostic relay using ordinary blocking TCP reads and writes with 32 KiB
//! buffers. Run with listen and backend addresses to compare against Tokio/RIO.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};

const BLOCK: usize = 32 * 1024;

fn buffer_override(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&kib| (64..=4096).contains(&kib))
        .map(|kib| kib * 1024)
}

fn copy(mut source: TcpStream, mut destination: TcpStream, direction: &str) -> io::Result<()> {
    let mut buffer = [0u8; BLOCK];
    let mut bytes = 0u64;
    let mut chunks = 0u64;
    loop {
        match source.read(&mut buffer) {
            Ok(0) => {
                destination.shutdown(Shutdown::Write)?;
                break;
            }
            Ok(size) => {
                destination.write_all(&buffer[..size])?;
                bytes += size as u64;
                chunks += 1;
            }
            Err(error) => return Err(error),
        }
    }
    if std::env::var_os("OI_BLOCKING_TRACE").is_some() {
        eprintln!("OI_BLOCKING_TRACE {direction} bytes={bytes} chunks={chunks}");
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let listen = args
        .next()
        .ok_or_else(|| io::Error::other("listen address missing"))?;
    let backend = args
        .next()
        .ok_or_else(|| io::Error::other("backend address missing"))?;
    let listener = TcpListener::bind(listen)?;
    for accepted in listener.incoming() {
        let client = accepted?;
        let address = backend.clone();
        let _ = std::thread::spawn(move || -> io::Result<()> {
            let server = TcpStream::connect(address)?;
            let common = buffer_override("OI_COPY_SOCKET_BUFFER_KIB");
            let receive = buffer_override("OI_COPY_RECV_BUFFER_KIB").or(common);
            let send = buffer_override("OI_COPY_SEND_BUFFER_KIB").or(common);
            let client_receive = buffer_override("OI_COPY_CLIENT_RECV_BUFFER_KIB").or(receive);
            let backend_receive = buffer_override("OI_COPY_BACKEND_RECV_BUFFER_KIB").or(receive);
            if client_receive.is_some() || backend_receive.is_some() || send.is_some() {
                for (socket, receive) in [(&client, client_receive), (&server, backend_receive)] {
                    let socket = socket2::SockRef::from(socket);
                    if let Some(size) = receive {
                        socket.set_recv_buffer_size(size)?;
                    }
                    if let Some(size) = send {
                        socket.set_send_buffer_size(size)?;
                    }
                }
            }
            let client_read = client.try_clone()?;
            let server_write = server.try_clone()?;
            let upstream =
                std::thread::spawn(move || copy(client_read, server_write, "client->backend"));
            let downstream = copy(server, client, "backend->client");
            let upstream = upstream
                .join()
                .map_err(|_| io::Error::other("copy thread panicked"))?;
            upstream?;
            downstream
        });
    }
    Ok(())
}
