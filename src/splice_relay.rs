//! Optional Linux TCP relay using splice through one nonblocking pipe per direction.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::Interest;
use tokio::net::{TcpListener, TcpStream};

const CHUNK: usize = 32 * 1024;

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut descriptors = [-1; 2];
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: unsafe { OwnedFd::from_raw_fd(descriptors[0]) },
            write: unsafe { OwnedFd::from_raw_fd(descriptors[1]) },
        })
    }
}

fn splice_once(input: RawFd, output: RawFd, length: usize) -> io::Result<usize> {
    let result = unsafe {
        libc::splice(
            input,
            std::ptr::null_mut(),
            output,
            std::ptr::null_mut(),
            length,
            libc::SPLICE_F_NONBLOCK,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

async fn splice_from_socket(source: &TcpStream, pipe_write: RawFd) -> io::Result<usize> {
    loop {
        source.readable().await?;
        match source.try_io(Interest::READABLE, || {
            splice_once(source.as_raw_fd(), pipe_write, CHUNK)
        }) {
            Ok(size) => return Ok(size),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

async fn splice_to_socket(
    pipe_read: RawFd,
    destination: &TcpStream,
    length: usize,
) -> io::Result<usize> {
    loop {
        destination.writable().await?;
        match destination.try_io(Interest::WRITABLE, || {
            splice_once(pipe_read, destination.as_raw_fd(), length)
        }) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(size) => return Ok(size),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

async fn copy_direction(source: &TcpStream, destination: &TcpStream) -> io::Result<u64> {
    let pipe = Pipe::new()?;
    let mut total = 0u64;
    loop {
        let size = splice_from_socket(source, pipe.write.as_raw_fd()).await?;
        if size == 0 {
            if unsafe { libc::shutdown(destination.as_raw_fd(), libc::SHUT_WR) } < 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(total);
        }
        let mut remaining = size;
        while remaining != 0 {
            remaining -= splice_to_socket(pipe.read.as_raw_fd(), destination, remaining).await?;
        }
        total += size as u64;
    }
}

async fn relay(client: TcpStream, backend: TcpStream) -> io::Result<(u64, u64)> {
    tokio::try_join!(
        copy_direction(&client, &backend),
        copy_direction(&backend, &client)
    )
}

pub async fn start(bind: SocketAddr, backend: String) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    loop {
        let (client, _) = listener.accept().await?;
        let address = backend.clone();
        tokio::spawn(async move {
            let result = async {
                let backend = TcpStream::connect(address).await?;
                relay(client, backend).await?;
                Ok::<(), io::Error>(())
            }
            .await;
            if let Err(error) = result {
                eprintln!("splice relay error: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn response_survives_client_half_close() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();

        let backend_task = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request");
            stream.write_all(b"response").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let proxy_task = tokio::spawn(async move {
            let (client, _) = proxy.accept().await.unwrap();
            let backend = TcpStream::connect(server_addr).await.unwrap();
            relay(client, backend).await.unwrap();
        });
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, b"response");
        timeout(Duration::from_secs(5), proxy_task)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), backend_task)
            .await
            .unwrap()
            .unwrap();
    }
}
