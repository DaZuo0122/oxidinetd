mod common;

use common::*;
use std::net::TcpStream;
use std::time::Duration;

fn tcp_proxy_config(bind_port: u16, connect_port: u16) -> String {
    format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "tcp"
"#,
        bind_port, connect_port
    )
}

fn write_proxy_config(port: u16, connect_port: u16) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, tcp_proxy_config(port, connect_port)).unwrap();
    (dir, path)
}

#[test]
fn graceful_shutdown_signal() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let (_dir, path) = write_proxy_config(port, echo.addr.port());

    #[cfg(unix)]
    let mut child = {
        use std::process::Stdio;
        std::process::Command::new(BIN)
            .arg("-c")
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oi binary")
    };

    #[cfg(windows)]
    let mut child = win::spawn_with_new_group(BIN, &["-c", path.to_str().unwrap()]);

    let bind_addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    assert!(wait_for_port(bind_addr, Duration::from_secs(10)));

    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT failed");
    }

    #[cfg(windows)]
    {
        assert!(
            win::send_ctrl_break(child.pid),
            "GenerateConsoleCtrlEvent failed: tests must run attached to a console"
        );
    }

    let exited = wait_for_exit(&mut child, Duration::from_secs(10));
    assert!(exited, "proxy did not shut down after signal");

    #[cfg(unix)]
    {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "proxy exited with {:?}", output.status.code());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Received Ctrl+C, shutting down..."),
            "expected Ctrl+C message, got: {}",
            stdout
        );
        assert!(
            stdout.contains("Server shut down successfully"),
            "expected clean shutdown message, got: {}",
            stdout
        );
    }

    #[cfg(windows)]
    {
        let code = child.wait_exit(0).unwrap();
        assert_eq!(code, 0, "proxy must exit with code 0 after graceful shutdown");
    }
}

#[test]
fn shutdown_during_active_connections() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let (_dir, path) = write_proxy_config(port, echo.addr.port());

    #[cfg(unix)]
    let mut child = {
        use std::process::Stdio;
        std::process::Command::new(BIN)
            .arg("-c")
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oi binary")
    };

    #[cfg(windows)]
    let mut child = win::spawn_with_new_group(BIN, &["-c", path.to_str().unwrap()]);

    let bind_addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    assert!(wait_for_port(bind_addr, Duration::from_secs(10)));

    // Hold an active connection through the proxy when the signal arrives.
    let mut stream = TcpStream::connect(bind_addr).expect("connect through proxy");
    use std::io::Write;
    stream.write_all(b"keep alive").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT failed");
    }

    #[cfg(windows)]
    {
        assert!(
            win::send_ctrl_break(child.pid),
            "GenerateConsoleCtrlEvent failed: tests must run attached to a console"
        );
    }

    let exited = wait_for_exit(&mut child, Duration::from_secs(10));
    assert!(exited, "proxy did not shut down with an active connection");

    #[cfg(unix)]
    {
        let status = child.wait().unwrap();
        assert!(status.success(), "proxy exited with {:?}", status.code());
    }

    #[cfg(windows)]
    {
        let code = child.wait_exit(0).unwrap();
        assert_eq!(code, 0);
    }

    drop(stream);
}

fn wait_for_exit<T>(child: &mut T, timeout: Duration) -> bool
where
    T: ExitProbe,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if child.has_exited() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

trait ExitProbe {
    fn has_exited(&mut self) -> bool;
}

impl ExitProbe for std::process::Child {
    fn has_exited(&mut self) -> bool {
        self.try_wait().ok().flatten().is_some()
    }
}

#[cfg(windows)]
impl ExitProbe for win::WinChild {
    fn has_exited(&mut self) -> bool {
        self.try_wait()
    }
}

#[cfg(windows)]
mod win {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use winapi::shared::minwindef::{DWORD, FALSE};
    use winapi::shared::winerror::WAIT_TIMEOUT;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{
        CreateProcessW, GetExitCodeProcess, TerminateProcess, PROCESS_INFORMATION,
    };
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::{CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT};
    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use winapi::um::processthreadsapi::STARTUPINFOW;

    pub struct WinChild {
        pub pid: u32,
        process: *mut winapi::ctypes::c_void,
        exited_code: Option<i32>,
    }

    impl Drop for WinChild {
        fn drop(&mut self) {
            unsafe {
                TerminateProcess(self.process, 1);
                CloseHandle(self.process);
            }
        }
    }

    impl WinChild {
        /// Wait up to `timeout_ms` for exit; returns the exit code.
        pub fn wait_exit(&mut self, timeout_ms: u32) -> Option<i32> {
            if let Some(code) = self.exited_code {
                return Some(code);
            }
            let result = unsafe { WaitForSingleObject(self.process, timeout_ms) };
            if result == WAIT_TIMEOUT {
                return None;
            }
            let mut code: DWORD = 0;
            unsafe {
                GetExitCodeProcess(self.process, &mut code);
            }
            self.exited_code = Some(code as i32);
            Some(code as i32)
        }

        pub fn try_wait(&mut self) -> bool {
            self.wait_exit(0).is_some()
        }
    }

    fn quote(s: &str) -> Vec<u16> {
        let mut v = Vec::new();
        v.push(b'"' as u16);
        v.extend(s.encode_utf16());
        v.push(b'"' as u16);
        v
    }

    /// Spawns a process as the leader of its own process group so that
    /// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` can reach it.
    pub fn spawn_with_new_group(exe: &str, args: &[&str]) -> WinChild {
        let mut cmdline: Vec<u16> = quote(exe);
        for arg in args {
            cmdline.push(b' ' as u16);
            cmdline.extend(quote(arg));
        }
        cmdline.push(0);

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = size_of::<STARTUPINFOW>() as u32;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let ok = unsafe {
            CreateProcessW(
                null(),
                cmdline.as_mut_ptr(),
                null_mut(),
                null_mut(),
                FALSE,
                CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
                null_mut(),
                null_mut(),
                &mut si,
                &mut pi,
            )
        };
        assert!(ok != 0, "CreateProcessW failed (win32 error {})", unsafe {
            GetLastError()
        });

        unsafe {
            CloseHandle(pi.hThread);
        }

        WinChild {
            pid: pi.dwProcessId,
            process: pi.hProcess,
            exited_code: None,
        }
    }

    /// Sends CTRL_BREAK to the process group rooted at `pid`. The ctrlc
    /// handler installed by the proxy covers both CTRL_C and CTRL_BREAK.
    pub fn send_ctrl_break(pid: u32) -> bool {
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
    }
}
