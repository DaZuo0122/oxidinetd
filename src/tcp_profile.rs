//! Optional TCP stage probe. Compiled only with `--features tcp-profile`.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Stage {
    Accepted,
    HandlerStarted,
    BackendConnectStarted,
    BackendConnected,
    CopyStarted,
    ClientRead,
    BackendWritten,
    BackendRead,
    ClientWritten,
}

#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub client_addr: SocketAddr,
    pub stage: Stage,
    pub at: Instant,
}

static EVENTS: OnceLock<UnboundedSender<Event>> = OnceLock::new();

pub fn install(sender: UnboundedSender<Event>) -> bool {
    EVENTS.set(sender).is_ok()
}

pub fn emit(client_addr: SocketAddr, stage: Stage) {
    if let Some(sender) = EVENTS.get() {
        let _ = sender.send(Event {
            client_addr,
            stage,
            at: Instant::now(),
        });
    }
}

pub struct ProfiledStream<'a> {
    inner: &'a mut TcpStream,
    client_addr: SocketAddr,
    read_stage: Stage,
    write_stage: Stage,
    first_read: bool,
    first_write: bool,
}

impl<'a> ProfiledStream<'a> {
    pub fn new(
        inner: &'a mut TcpStream,
        client_addr: SocketAddr,
        read_stage: Stage,
        write_stage: Stage,
    ) -> Self {
        Self {
            inner,
            client_addr,
            read_stage,
            write_stage,
            first_read: true,
            first_write: true,
        }
    }
}

impl AsyncRead for ProfiledStream<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut *self.inner).poll_read(cx, buf);
        if self.first_read && matches!(&result, Poll::Ready(Ok(()))) && buf.filled().len() > before
        {
            self.first_read = false;
            emit(self.client_addr, self.read_stage);
        }
        result
    }
}

impl AsyncWrite for ProfiledStream<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut *self.inner).poll_write(cx, buf);
        if self.first_write && matches!(&result, Poll::Ready(Ok(n)) if *n > 0) {
            self.first_write = false;
            emit(self.client_addr, self.write_stage);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}
