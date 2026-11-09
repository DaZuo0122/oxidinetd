//! Experimental Windows RIO TCP relay. Tokio schedules each connection; a
//! registered Windows wait wakes the task when its RIO queue has completions.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::windows::io::AsRawSocket;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use windows_sys::Win32::Foundation::{
    CloseHandle, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    RIO_BUF, RIO_CORRUPT_CQ, RIO_EVENT_COMPLETION, RIO_EXTENSION_FUNCTION_TABLE,
    RIO_NOTIFICATION_COMPLETION, RIO_NOTIFICATION_COMPLETION_0, RIO_NOTIFICATION_COMPLETION_0_0,
    RIORESULT, SD_SEND, SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER, WSAGetLastError,
    WSAID_MULTIPLE_RIO, WSAIoctl, shutdown,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEDEFAULT,
    WaitForSingleObject,
};

const DEFAULT_BLOCK: usize = 32 * 1024;
const RECV_CLIENT: u64 = 1;
const RECV_BACKEND: u64 = 2;
const SEND_BACKEND: u64 = 3;
const SEND_CLIENT: u64 = 4;

fn wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[derive(Clone, Copy)]
struct RioOptions {
    block: usize,
    nodelay: bool,
}

impl RioOptions {
    fn from_environment() -> Self {
        let block = std::env::var("OI_RIO_BUFFER_KIB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&kib| (8..=256).contains(&kib))
            .map(|kib| kib * 1024)
            .unwrap_or(DEFAULT_BLOCK);
        let nodelay = std::env::var("OI_RIO_NODELAY").as_deref() == Ok("1")
            || std::env::var("OI_TCP_NODELAY").as_deref() == Ok("1");
        Self { block, nodelay }
    }
}

struct RioEventWait {
    wait_handle: usize,
    context: usize,
    notification: Arc<RioNotification>,
}

struct RioNotification {
    wake: Notify,
    generation: AtomicU64,
}

unsafe extern "system" fn wake_rio_task(context: *mut std::ffi::c_void, _: bool) {
    let notification = unsafe { &*(context as *const RioNotification) };
    notification.generation.fetch_add(1, Ordering::Release);
    notification.wake.notify_one();
}

impl RioEventWait {
    fn register(event: usize) -> io::Result<Self> {
        let notification = Arc::new(RioNotification {
            wake: Notify::new(),
            generation: AtomicU64::new(0),
        });
        let context = Arc::into_raw(notification.clone()) as usize;
        let mut wait_handle = ptr::null_mut();
        let status = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_handle,
                event as _,
                Some(wake_rio_task),
                context as _,
                INFINITE,
                WT_EXECUTEDEFAULT,
            )
        };
        if status == 0 {
            let error = io::Error::last_os_error();
            unsafe { drop(Arc::from_raw(context as *const RioNotification)) };
            return Err(error);
        }
        Ok(Self {
            wait_handle: wait_handle as usize,
            context,
            notification,
        })
    }

    fn unregister(&mut self) -> bool {
        if self.wait_handle == 0 {
            return true;
        }
        if unsafe { UnregisterWaitEx(self.wait_handle as _, INVALID_HANDLE_VALUE) } == 0 {
            return false;
        }
        self.wait_handle = 0;
        unsafe { drop(Arc::from_raw(self.context as *const RioNotification)) };
        true
    }
}

impl Drop for RioEventWait {
    fn drop(&mut self) {
        let _ = self.unregister();
    }
}

fn rio_socket(addr: SocketAddr) -> io::Result<Socket> {
    Socket::new(
        if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        },
        Type::STREAM.registered_io(),
        Some(Protocol::TCP),
    )
}

fn functions(socket: &Socket) -> io::Result<RIO_EXTENSION_FUNCTION_TABLE> {
    let mut table: RIO_EXTENSION_FUNCTION_TABLE = unsafe { zeroed() };
    table.cbSize = size_of::<RIO_EXTENSION_FUNCTION_TABLE>() as u32;
    let mut returned = 0u32;
    let status = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER,
            &WSAID_MULTIPLE_RIO as *const _ as _,
            size_of_val(&WSAID_MULTIPLE_RIO) as u32,
            &mut table as *mut _ as _,
            size_of::<RIO_EXTENSION_FUNCTION_TABLE>() as u32,
            &mut returned,
            ptr::null_mut(),
            None,
        )
    };
    if status != 0 {
        return Err(wsa_error());
    }
    Ok(table)
}

fn size_of_val<T>(_: &T) -> usize {
    size_of::<T>()
}

pub async fn start(bind: SocketAddr, backend: String) -> io::Result<()> {
    let options = RioOptions::from_environment();
    let listener = rio_socket(bind)?;
    listener.bind(&SockAddr::from(bind))?;
    listener.listen(1024)?;
    let mut listener = listener;
    loop {
        let (next_listener, accepted) = tokio::task::spawn_blocking(move || {
            let accepted = listener.accept().map(|(socket, _)| socket);
            (listener, accepted)
        })
        .await
        .map_err(io::Error::other)?;
        listener = next_listener;
        let accepted = accepted?;
        let address = backend.clone();
        tokio::spawn(async move {
            if let Err(error) = relay(accepted, &address, options).await {
                eprintln!("RIO relay error: {error}");
            }
        });
    }
}

async fn relay(client: Socket, address: &str, options: RioOptions) -> io::Result<()> {
    if options.nodelay {
        client.set_tcp_nodelay(true)?;
    }
    let addr = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("backend address empty"))?;
    let backend = tokio::task::spawn_blocking(move || {
        let backend = rio_socket(addr)?;
        backend.connect(&SockAddr::from(addr))?;
        Ok::<_, io::Error>(backend)
    })
    .await
    .map_err(io::Error::other)??;
    if options.nodelay {
        backend.set_tcp_nodelay(true)?;
    }
    let mut session = RioSession::new(client, backend, options.block)?;
    session.run().await
}

struct RioSession {
    client: Option<Socket>,
    backend: Option<Socket>,
    api: RIO_EXTENSION_FUNCTION_TABLE,
    event: usize,
    cq: isize,
    waiter: Option<RioEventWait>,
    from_client: Vec<u8>,
    from_backend: Vec<u8>,
    client_id: isize,
    backend_id: isize,
    block: usize,
    outstanding: u32,
}

impl RioSession {
    fn new(client: Socket, backend: Socket, block: usize) -> io::Result<Self> {
        let api = functions(&client)?;
        let (Some(create_cq), Some(close_cq), Some(register), Some(deregister)) = (
            api.RIOCreateCompletionQueue,
            api.RIOCloseCompletionQueue,
            api.RIORegisterBuffer,
            api.RIODeregisterBuffer,
        ) else {
            return Err(io::Error::other("RIO API table incomplete"));
        };
        let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) } as usize;
        if event == 0 {
            return Err(io::Error::last_os_error());
        }
        let cq = {
            let completion = RIO_NOTIFICATION_COMPLETION {
                Type: RIO_EVENT_COMPLETION,
                Anonymous: RIO_NOTIFICATION_COMPLETION_0 {
                    Event: RIO_NOTIFICATION_COMPLETION_0_0 {
                        EventHandle: event as _,
                        NotifyReset: 0,
                    },
                },
            };
            unsafe { create_cq(16, &completion) }
        };
        if cq == 0 {
            let error = wsa_error();
            unsafe {
                CloseHandle(event as _);
            }
            return Err(error);
        }
        let mut from_client = vec![0u8; block * 2];
        let mut from_backend = vec![0u8; block * 2];
        let client_id = unsafe { register(from_client.as_mut_ptr(), (block * 2) as u32) };
        if client_id == 0 {
            let error = wsa_error();
            unsafe {
                close_cq(cq);
                CloseHandle(event as _);
            }
            return Err(error);
        }
        let backend_id = unsafe { register(from_backend.as_mut_ptr(), (block * 2) as u32) };
        if backend_id == 0 {
            let error = wsa_error();
            unsafe {
                deregister(client_id);
                close_cq(cq);
                CloseHandle(event as _);
            }
            return Err(error);
        }
        let waiter = match RioEventWait::register(event) {
            Ok(waiter) => waiter,
            Err(error) => {
                unsafe {
                    deregister(client_id);
                    deregister(backend_id);
                    close_cq(cq);
                    CloseHandle(event as _);
                }
                return Err(error);
            }
        };
        Ok(Self {
            client: Some(client),
            backend: Some(backend),
            api,
            event,
            cq,
            waiter: Some(waiter),
            from_client,
            from_backend,
            client_id,
            backend_id,
            block,
            outstanding: 0,
        })
    }

    async fn run(&mut self) -> io::Result<()> {
        relay_with_queue_body(self).await
    }
}

impl Drop for RioSession {
    fn drop(&mut self) {
        // A Tokio task can be cancelled while waiting for a completion. Stop
        // callbacks, close sockets, and drain cancellation completions before
        // deregistering or freeing any memory used by pending requests.
        if let Some(mut waiter) = self.waiter.take() {
            if !waiter.unregister() {
                // Keep the event, CQ, callback context and buffers alive if the
                // wait could still dispatch a callback.
                drop(self.client.take());
                drop(self.backend.take());
                std::mem::forget(std::mem::take(&mut self.from_client));
                std::mem::forget(std::mem::take(&mut self.from_backend));
                std::mem::forget(waiter);
                return;
            }
        }
        drop(self.client.take());
        drop(self.backend.take());
        if drain_completions(&self.api, self.cq, self.event as _, &mut self.outstanding).is_err() {
            // With a broken CQ there is no proof that the kernel has stopped
            // accessing these buffers. Retain them and the queue until exit.
            std::mem::forget(std::mem::take(&mut self.from_client));
            std::mem::forget(std::mem::take(&mut self.from_backend));
            return;
        }
        unsafe {
            if let Some(deregister) = self.api.RIODeregisterBuffer {
                deregister(self.client_id);
                deregister(self.backend_id);
            }
            if let Some(close_cq) = self.api.RIOCloseCompletionQueue {
                close_cq(self.cq);
            }
            CloseHandle(self.event as _);
        }
    }
}

struct Direction {
    source_rq: isize,
    destination_rq: isize,
    destination_socket: usize,
    buffers: [RIO_BUF; 2],
    receive_context: u64,
    send_context: u64,
    receiving: Option<usize>,
    sending: Option<(usize, u32, u32)>,
    ready: Option<(usize, u32)>,
    eof: bool,
    shut_down: bool,
}

impl Direction {
    fn post_receive(
        &mut self,
        api: &RIO_EXTENSION_FUNCTION_TABLE,
        slot: usize,
        outstanding: &mut u32,
    ) -> io::Result<()> {
        let receive = api
            .RIOReceive
            .ok_or_else(|| io::Error::other("RIOReceive missing"))?;
        let context = self.receive_context + (slot as u64) * 4;
        if unsafe { receive(self.source_rq, &self.buffers[slot], 1, 0, context as _) } == 0 {
            return Err(wsa_error());
        }
        self.receiving = Some(slot);
        *outstanding += 1;
        Ok(())
    }

    fn post_send(
        &mut self,
        api: &RIO_EXTENSION_FUNCTION_TABLE,
        slot: usize,
        len: u32,
        sent: u32,
        outstanding: &mut u32,
    ) -> io::Result<()> {
        let send = api
            .RIOSend
            .ok_or_else(|| io::Error::other("RIOSend missing"))?;
        let buffer = RIO_BUF {
            Offset: self.buffers[slot].Offset + sent,
            Length: len - sent,
            ..self.buffers[slot]
        };
        let context = self.send_context + (slot as u64) * 4;
        if unsafe { send(self.destination_rq, &buffer, 1, 0, context as _) } == 0 {
            return Err(wsa_error());
        }
        self.sending = Some((slot, len, sent));
        *outstanding += 1;
        Ok(())
    }

    fn refill(
        &mut self,
        api: &RIO_EXTENSION_FUNCTION_TABLE,
        outstanding: &mut u32,
    ) -> io::Result<()> {
        if self.eof || self.receiving.is_some() {
            return Ok(());
        }
        let free = (0..2).find(|slot| {
            self.sending.is_none_or(|(active, _, _)| active != *slot)
                && self.ready.is_none_or(|(active, _)| active != *slot)
        });
        if let Some(slot) = free {
            self.post_receive(api, slot, outstanding)?;
        }
        Ok(())
    }

    fn finish_if_eof(&mut self) {
        if self.eof && self.sending.is_none() && self.ready.is_none() && !self.shut_down {
            unsafe {
                shutdown(self.destination_socket, SD_SEND);
            }
            self.shut_down = true;
        }
    }

    fn on_receive(
        &mut self,
        api: &RIO_EXTENSION_FUNCTION_TABLE,
        slot: usize,
        bytes: u32,
        outstanding: &mut u32,
    ) -> io::Result<()> {
        if self.receiving != Some(slot) || bytes > self.buffers[slot].Length {
            return Err(io::Error::other("unexpected RIO receive completion"));
        }
        self.receiving = None;
        if bytes == 0 {
            self.eof = true;
        } else if self.sending.is_none() {
            self.post_send(api, slot, bytes, 0, outstanding)?;
        } else {
            if self.ready.is_some() {
                return Err(io::Error::other("RIO receive buffer queue full"));
            }
            self.ready = Some((slot, bytes));
        }
        self.refill(api, outstanding)?;
        self.finish_if_eof();
        Ok(())
    }

    fn on_send(
        &mut self,
        api: &RIO_EXTENSION_FUNCTION_TABLE,
        slot: usize,
        bytes: u32,
        outstanding: &mut u32,
    ) -> io::Result<()> {
        let Some((active, len, sent)) = self.sending else {
            return Err(io::Error::other("unexpected RIO send completion"));
        };
        if active != slot || bytes == 0 || bytes > len - sent {
            return Err(io::Error::other("invalid RIO send completion"));
        }
        let sent = sent + bytes;
        if sent < len {
            self.post_send(api, slot, len, sent, outstanding)?;
        } else {
            self.sending = None;
            if let Some((next_slot, next_len)) = self.ready.take() {
                self.post_send(api, next_slot, next_len, 0, outstanding)?;
            }
            self.refill(api, outstanding)?;
            self.finish_if_eof();
        }
        Ok(())
    }
}

async fn relay_with_queue_body(session: &mut RioSession) -> io::Result<()> {
    let client = session.client.as_ref().expect("RIO client socket");
    let backend = session.backend.as_ref().expect("RIO backend socket");
    let api = &session.api;
    let cq = session.cq;
    let notification = &session
        .waiter
        .as_ref()
        .expect("RIO event wait")
        .notification;
    let client_buf = [0, session.block as u32].map(|offset| RIO_BUF {
        BufferId: session.client_id,
        Offset: offset,
        Length: session.block as u32,
    });
    let backend_buf = [0, session.block as u32].map(|offset| RIO_BUF {
        BufferId: session.backend_id,
        Offset: offset,
        Length: session.block as u32,
    });
    let outstanding = &mut session.outstanding;
    let create_rq = api
        .RIOCreateRequestQueue
        .ok_or_else(|| io::Error::other("RIOCreateRequestQueue missing"))?;
    let dequeue = api
        .RIODequeueCompletion
        .ok_or_else(|| io::Error::other("RIODequeueCompletion missing"))?;
    let notify = api
        .RIONotify
        .ok_or_else(|| io::Error::other("RIONotify missing"))?;
    let client_rq = unsafe {
        create_rq(
            client.as_raw_socket() as usize,
            1,
            1,
            1,
            1,
            cq,
            cq,
            ptr::null(),
        )
    };
    if client_rq == 0 {
        return Err(wsa_error());
    }
    let backend_rq = unsafe {
        create_rq(
            backend.as_raw_socket() as usize,
            1,
            1,
            1,
            1,
            cq,
            cq,
            ptr::null(),
        )
    };
    if backend_rq == 0 {
        return Err(wsa_error());
    }
    let mut client_to_backend = Direction {
        source_rq: client_rq,
        destination_rq: backend_rq,
        destination_socket: backend.as_raw_socket() as usize,
        buffers: client_buf,
        receive_context: RECV_CLIENT,
        send_context: SEND_BACKEND,
        receiving: None,
        sending: None,
        ready: None,
        eof: false,
        shut_down: false,
    };
    let mut backend_to_client = Direction {
        source_rq: backend_rq,
        destination_rq: client_rq,
        destination_socket: client.as_raw_socket() as usize,
        buffers: backend_buf,
        receive_context: RECV_BACKEND,
        send_context: SEND_CLIENT,
        receiving: None,
        sending: None,
        ready: None,
        eof: false,
        shut_down: false,
    };
    client_to_backend.refill(api, outstanding)?;
    backend_to_client.refill(api, outstanding)?;
    let mut results = [RIORESULT::default(); 8];
    let mut observed_notification = notification.generation.load(Ordering::Acquire);
    while !client_to_backend.shut_down || !backend_to_client.shut_down {
        let count = unsafe { dequeue(cq, results.as_mut_ptr(), results.len() as u32) };
        if count == RIO_CORRUPT_CQ {
            return Err(io::Error::other("RIO completion queue corrupted"));
        }
        if count == 0 {
            if unsafe { notify(cq) } != 0 {
                return Err(wsa_error());
            }
            while notification.generation.load(Ordering::Acquire) == observed_notification {
                notification.wake.notified().await;
            }
            observed_notification = notification.generation.load(Ordering::Acquire);
            continue;
        }
        if count > *outstanding {
            return Err(io::Error::other(
                "RIO completion count exceeds posted requests",
            ));
        }
        *outstanding -= count;
        for result in results.iter().take(count as usize) {
            if result.Status != 0 {
                return Err(io::Error::from_raw_os_error(result.Status));
            }
            if result.RequestContext == 0 {
                return Err(io::Error::other("unknown RIO completion context"));
            }
            let slot = ((result.RequestContext - 1) / 4) as usize;
            if slot >= 2 {
                return Err(io::Error::other("unknown RIO completion slot"));
            }
            match result.RequestContext - (slot as u64) * 4 {
                RECV_CLIENT => {
                    client_to_backend.on_receive(api, slot, result.BytesTransferred, outstanding)?
                }
                RECV_BACKEND => {
                    backend_to_client.on_receive(api, slot, result.BytesTransferred, outstanding)?
                }
                SEND_BACKEND => {
                    client_to_backend.on_send(api, slot, result.BytesTransferred, outstanding)?
                }
                SEND_CLIENT => {
                    backend_to_client.on_send(api, slot, result.BytesTransferred, outstanding)?
                }
                _ => return Err(io::Error::other("unknown RIO completion")),
            }
        }
    }
    Ok(())
}

fn drain_completions(
    api: &RIO_EXTENSION_FUNCTION_TABLE,
    cq: isize,
    event: windows_sys::Win32::Foundation::HANDLE,
    outstanding: &mut u32,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let dequeue = api
        .RIODequeueCompletion
        .ok_or_else(|| io::Error::other("RIODequeueCompletion missing"))?;
    let notify = api
        .RIONotify
        .ok_or_else(|| io::Error::other("RIONotify missing"))?;
    let mut results = [RIORESULT::default(); 8];
    while *outstanding != 0 {
        let count = unsafe { dequeue(cq, results.as_mut_ptr(), results.len() as u32) };
        if count == RIO_CORRUPT_CQ || count > *outstanding {
            return Err(io::Error::other("RIO completion queue cannot be drained"));
        }
        if count != 0 {
            *outstanding -= count;
            continue;
        }
        if unsafe { notify(cq) } != 0 {
            return Err(wsa_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RIO drain timed out",
            ));
        }
        match unsafe { WaitForSingleObject(event, remaining.as_millis().max(1) as u32) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RIO drain timed out",
                ));
            }
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_response_after_client_half_close() {
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let backend_addr = backend.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = backend.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            std::io::Read::read_to_end(&mut stream, &mut request).unwrap();
            assert_eq!(request, b"request");
            std::io::Write::write_all(&mut stream, b"response").unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });

        let listener = rio_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        listener
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        listener.listen(1).unwrap();
        let listen_addr = listener.local_addr().unwrap().as_socket().unwrap();
        let mut client = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let accepted = listener.accept().unwrap().0;
        let relay_task = tokio::spawn(async move {
            relay(
                accepted,
                &backend_addr,
                RioOptions {
                    block: DEFAULT_BLOCK,
                    nodelay: false,
                },
            )
            .await
        });
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, b"response");
        tokio::time::timeout(Duration::from_secs(5), relay_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_drains_pending_receives() {
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let backend_addr = backend.local_addr().unwrap().to_string();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let peers: Vec<_> = (0..8).map(|_| backend.accept().unwrap().0).collect();
            let _ = wait.recv_timeout(Duration::from_secs(5));
            drop(peers);
        });
        let listener = rio_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        listener
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        listener.listen(8).unwrap();
        let listen_addr = listener.local_addr().unwrap().as_socket().unwrap();
        let mut clients = Vec::new();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            clients.push(TcpStream::connect(listen_addr).unwrap());
            let accepted = listener.accept().unwrap().0;
            let address = backend_addr.clone();
            tasks.push(tokio::spawn(async move {
                relay(
                    accepted,
                    &address,
                    RioOptions {
                        block: DEFAULT_BLOCK,
                        nodelay: false,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap_err();
        }
        drop(clients);
        release.send(()).unwrap();
        server.join().unwrap();
    }
}
