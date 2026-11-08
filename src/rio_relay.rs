//! Experimental Windows RIO TCP relay. Tokio owns task scheduling; one
//! blocking worker owns each connection's RIO queues and registered buffers.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::windows::io::AsRawSocket;
use std::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::Networking::WinSock::{
    RIO_BUF, RIO_CORRUPT_CQ, RIO_EVENT_COMPLETION, RIO_EXTENSION_FUNCTION_TABLE,
    RIO_NOTIFICATION_COMPLETION, RIO_NOTIFICATION_COMPLETION_0, RIO_NOTIFICATION_COMPLETION_0_0,
    RIORESULT, SD_SEND, SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER, WSAID_MULTIPLE_RIO, WSAIoctl,
    shutdown,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

const BLOCK: usize = 64 * 1024;
const RECV_CLIENT: u64 = 1;
const RECV_BACKEND: u64 = 2;
const SEND_BACKEND: u64 = 3;
const SEND_CLIENT: u64 = 4;

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
        return Err(io::Error::last_os_error());
    }
    Ok(table)
}

fn size_of_val<T>(_: &T) -> usize {
    size_of::<T>()
}

pub async fn start(bind: SocketAddr, backend: String) -> io::Result<()> {
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
            let result = tokio::task::spawn_blocking(move || relay(accepted, &address)).await;
            if let Ok(Err(error)) = result {
                eprintln!("RIO relay error: {error}");
            }
        });
    }
}

fn relay(client: Socket, address: &str) -> io::Result<()> {
    let addr = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("backend address empty"))?;
    let backend = rio_socket(addr)?;
    backend.connect(&SockAddr::from(addr))?;
    let api = functions(&client)?;
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let completion = RIO_NOTIFICATION_COMPLETION {
        Type: RIO_EVENT_COMPLETION,
        Anonymous: RIO_NOTIFICATION_COMPLETION_0 {
            Event: RIO_NOTIFICATION_COMPLETION_0_0 {
                EventHandle: event,
                NotifyReset: 1,
            },
        },
    };
    let (Some(create_cq), Some(close_cq)) =
        (api.RIOCreateCompletionQueue, api.RIOCloseCompletionQueue)
    else {
        unsafe { CloseHandle(event) };
        return Err(io::Error::other("RIO completion queue API missing"));
    };
    let cq = unsafe { create_cq(16, &completion) };
    if cq == 0 {
        unsafe {
            CloseHandle(event);
        }
        return Err(io::Error::last_os_error());
    }
    let (result, safe_to_close) = relay_with_queue(client, backend, &api, cq, event);
    if safe_to_close {
        unsafe {
            close_cq(cq);
            CloseHandle(event);
        }
    }
    result
}

fn relay_with_queue(
    client: Socket,
    backend: Socket,
    api: &RIO_EXTENSION_FUNCTION_TABLE,
    cq: isize,
    event: windows_sys::Win32::Foundation::HANDLE,
) -> (io::Result<()>, bool) {
    let mut from_client = vec![0u8; BLOCK];
    let mut from_backend = vec![0u8; BLOCK];
    let (Some(register), Some(deregister)) = (api.RIORegisterBuffer, api.RIODeregisterBuffer)
    else {
        return (Err(io::Error::other("RIO buffer API missing")), true);
    };
    let client_id = unsafe { register(from_client.as_mut_ptr(), BLOCK as u32) };
    let backend_id = unsafe { register(from_backend.as_mut_ptr(), BLOCK as u32) };
    if client_id == 0 || backend_id == 0 {
        let error = io::Error::last_os_error();
        if client_id != 0 {
            unsafe {
                deregister(client_id);
            }
        }
        if backend_id != 0 {
            unsafe {
                deregister(backend_id);
            }
        }
        return (Err(error), true);
    }
    let client_buf = RIO_BUF {
        BufferId: client_id,
        Offset: 0,
        Length: BLOCK as u32,
    };
    let backend_buf = RIO_BUF {
        BufferId: backend_id,
        Offset: 0,
        Length: BLOCK as u32,
    };
    let mut outstanding = 0u32;
    let result = relay_with_queue_body(
        &client,
        &backend,
        api,
        cq,
        event,
        client_buf,
        backend_buf,
        &mut outstanding,
    );
    // Closing both sockets cancels any pending operations. Keep the CQ and
    // registered memory alive until their cancellation completions arrive.
    drop(client);
    drop(backend);
    let drain = drain_completions(api, cq, event, &mut outstanding);
    if let Err(error) = drain {
        // A broken CQ leaves no way to prove that the kernel has stopped using
        // these buffers. Retain the memory and queue for process lifetime.
        std::mem::forget(from_client);
        std::mem::forget(from_backend);
        return (Err(error), false);
    }
    unsafe {
        deregister(client_id);
        deregister(backend_id);
    }
    (result, true)
}

fn relay_with_queue_body(
    client: &Socket,
    backend: &Socket,
    api: &RIO_EXTENSION_FUNCTION_TABLE,
    cq: isize,
    event: windows_sys::Win32::Foundation::HANDLE,
    client_buf: RIO_BUF,
    backend_buf: RIO_BUF,
    outstanding: &mut u32,
) -> io::Result<()> {
    let create_rq = api
        .RIOCreateRequestQueue
        .ok_or_else(|| io::Error::other("RIOCreateRequestQueue missing"))?;
    let receive = api
        .RIOReceive
        .ok_or_else(|| io::Error::other("RIOReceive missing"))?;
    let send = api
        .RIOSend
        .ok_or_else(|| io::Error::other("RIOSend missing"))?;
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
    if client_rq == 0 || backend_rq == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pending_client = 0u32;
    let mut pending_backend = 0u32;
    let mut sent_client = 0u32;
    let mut sent_backend = 0u32;
    let mut done_client = false;
    let mut done_backend = false;
    if unsafe { receive(client_rq, &client_buf, 1, 0, RECV_CLIENT as _) } == 0 {
        return Err(io::Error::last_os_error());
    }
    *outstanding += 1;
    if unsafe { receive(backend_rq, &backend_buf, 1, 0, RECV_BACKEND as _) } == 0 {
        return Err(io::Error::last_os_error());
    }
    *outstanding += 1;
    let mut results = [RIORESULT::default(); 8];
    while !done_client || !done_backend {
        let count = unsafe { dequeue(cq, results.as_mut_ptr(), results.len() as u32) };
        if count == RIO_CORRUPT_CQ {
            return Err(io::Error::other("RIO completion queue corrupted"));
        }
        if count == 0 {
            if unsafe { notify(cq) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
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
            match result.RequestContext {
                RECV_CLIENT => {
                    if result.BytesTransferred == 0 {
                        done_client = true;
                        unsafe {
                            shutdown(backend.as_raw_socket() as usize, SD_SEND);
                        }
                    } else {
                        pending_client = result.BytesTransferred;
                        sent_client = 0;
                        let buf = RIO_BUF {
                            Length: pending_client,
                            ..client_buf
                        };
                        if unsafe { send(backend_rq, &buf, 1, 0, SEND_BACKEND as _) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    }
                }
                RECV_BACKEND => {
                    if result.BytesTransferred == 0 {
                        done_backend = true;
                        unsafe {
                            shutdown(client.as_raw_socket() as usize, SD_SEND);
                        }
                    } else {
                        pending_backend = result.BytesTransferred;
                        sent_backend = 0;
                        let buf = RIO_BUF {
                            Length: pending_backend,
                            ..backend_buf
                        };
                        if unsafe { send(client_rq, &buf, 1, 0, SEND_CLIENT as _) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    }
                }
                SEND_BACKEND => {
                    if result.BytesTransferred == 0 {
                        return Err(io::Error::from(io::ErrorKind::WriteZero));
                    }
                    sent_client += result.BytesTransferred;
                    if sent_client < pending_client {
                        let buf = RIO_BUF {
                            Offset: sent_client,
                            Length: pending_client - sent_client,
                            ..client_buf
                        };
                        if unsafe { send(backend_rq, &buf, 1, 0, SEND_BACKEND as _) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    } else {
                        if unsafe { receive(client_rq, &client_buf, 1, 0, RECV_CLIENT as _) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    }
                }
                SEND_CLIENT => {
                    if result.BytesTransferred == 0 {
                        return Err(io::Error::from(io::ErrorKind::WriteZero));
                    }
                    sent_backend += result.BytesTransferred;
                    if sent_backend < pending_backend {
                        let buf = RIO_BUF {
                            Offset: sent_backend,
                            Length: pending_backend - sent_backend,
                            ..backend_buf
                        };
                        if unsafe { send(client_rq, &buf, 1, 0, SEND_CLIENT as _) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    } else {
                        if unsafe { receive(backend_rq, &backend_buf, 1, 0, RECV_BACKEND as _) }
                            == 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                        *outstanding += 1;
                    }
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
            return Err(io::Error::last_os_error());
        }
        if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
