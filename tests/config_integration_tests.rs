mod common;

use common::*;
use std::time::Duration;

#[test]
fn toml_config_full_startup() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let config = format!(
        r#"
log_file = "/tmp/oi.log"
pid_file = "/tmp/oi.pid"
log_format = "rinetd"

[[global_rules]]
type = "allow"
pattern = "127.0.0.*"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
protocol = "tcp"
timeout = 60

[[forwarding_rules.rules]]
type = "allow"
pattern = "127.0.0.1"
"#,
        port, echo.addr.port()
    );
    let mut proxy = spawn_proxy(&config);
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"full config");
    assert_eq!(response, b"full config");
    assert!(proxy.is_alive());
}

#[test]
fn legacy_config_startup() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let conf = format!(
        "# rinetd legacy config\nallow 127.0.0.*\n127.0.0.1 {} 127.0.0.1 {}\n",
        port,
        echo.addr.port()
    );
    let mut proxy = spawn_proxy_from_legacy_conf(&conf);
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"legacy config works");
    assert_eq!(response, b"legacy config works");
    assert!(proxy.is_alive());
}

#[test]
fn config_with_global_access_rules() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let config = format!(
        r#"
[[global_rules]]
type = "allow"
pattern = "10.0.0.0/8"

[[global_rules]]
type = "deny"
pattern = "192.168.1.*"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
"#,
        port, echo.addr.port()
    );
    let mut proxy = spawn_proxy(&config);
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));
    let response = tcp_round_trip(proxy.bind_addr, b"with global rules");
    assert_eq!(response, b"with global rules");
    assert!(proxy.is_alive());
}

#[test]
fn config_missing_forwarding_rules_exits_with_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, "log_file = \"/tmp/oi.log\"\n").unwrap();

    let output = std::process::Command::new(BIN)
        .arg("-c")
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run oi binary");

    assert!(!output.status.success(), "expected non-zero exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error loading config"),
        "expected config error on stderr, got: {}",
        stderr
    );
}

#[test]
fn invalid_bind_address_rule_is_skipped() {
    // A rule whose bind address does not parse must be skipped, while other
    // rules keep working. The valid rule comes first so the helper picks its
    // bind port for readiness checks.
    let echo = spawn_tcp_echo_server();
    let good_port = reserve_proxy_port();
    let bad_port = reserve_proxy_port();
    let config = format!(
        r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}

[[forwarding_rules]]
bind_address = "not-an-address!!!"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = 1234
"#,
        good_port, echo.addr.port(), bad_port
    );
    let mut proxy = spawn_proxy(&config);
    assert!(wait_for_port(proxy.bind_addr, Duration::from_secs(10)));

    let response = tcp_round_trip(proxy.bind_addr, b"good rule works");
    assert_eq!(response, b"good rule works");
    assert!(proxy.is_alive());
}

#[test]
fn verbose_flag_prints_loading_message() {
    let echo = spawn_tcp_echo_server();
    let port = reserve_proxy_port();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = {}
connect_address = "127.0.0.1"
connect_port = {}
"#,
            port, echo.addr.port()
        ),
    )
    .unwrap();

    let output = std::process::Command::new(BIN)
        .arg("-c")
        .arg(&path)
        .arg("-v")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn oi binary");

    std::thread::sleep(Duration::from_millis(800));

    let mut output = output;
    output.kill().unwrap();
    let captured = output.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&captured.stdout);
    assert!(
        stdout.contains("Loading configuration from"),
        "expected verbose loading message, got: {}",
        stdout
    );
    assert!(stdout.contains("Loaded 1 forwarding rules"));
    assert!(stdout.contains("Starting TCP forwarding"));
}

#[test]
fn no_rules_config_stays_alive() {
    // A valid config with zero forwarding rules keeps the process running
    // (waiting for shutdown) instead of crashing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, "forwarding_rules = []\n").unwrap();

    let mut child = std::process::Command::new(BIN)
        .arg("-c")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn oi binary");

    std::thread::sleep(Duration::from_millis(800));
    assert!(child.try_wait().unwrap().is_none(), "proxy exited with zero rules");

    child.kill().unwrap();
    let _ = child.wait();
}
