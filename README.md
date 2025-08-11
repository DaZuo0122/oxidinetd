# oxidinetd

A modern, lightweight port forwarding tool written in Rust, inspired by rinetd. This tool efficiently redirects connections from one IP address/port combination to another, supporting both traditional TCP/UDP forwarding and cross-protocol forwarding (UDP-to-TCP and TCP-to-UDP).

## Features

- **TCP Forwarding**: Standard TCP port forwarding
- **UDP Forwarding**: UDP port forwarding with connection tracking
- **Cross-Protocol Forwarding**:
  - UDP-to-TCP: Forward UDP packets as TCP stream data
  - TCP-to-UDP: Forward TCP connections as UDP packets
- **TOML Configuration**: Modern, human-readable configuration format
- **Legacy Compatibility**: Supports original rinetd .conf format
- **Access Control**: IP-based allow/deny rules
- **Lightweight**: Built with Smol async runtime for minimal overhead
- **Cross-Platform**: Runs on Linux, macOS, and Windows

## Installation
Download pre-built binaries at [release page](https://github.com/DaZuo0122/oxidinetd/releases)

### From Source

```bash
# Clone the repository
git clone https://github.com/DaZuo0122/oxidinetd.git
cd oxidinetd

# Build the project
cargo build --release

# The binary will be located at target/release/oi
```

### Prerequisites

- Rust edition 2021 or higher
- Cargo package manager

## Usage

```bash
# Basic usage
oi --config config.toml

# With verbose output
oi --config config.toml --verbose
```

## Configuration

### TOML Format (Recommended)

Create a `config.toml` file:

```toml
# Global access rules (applied to all forwarding rules)
[[global_rules]]
type = "allow"
pattern = "192.168.1.*"

[[global_rules]]
type = "deny"
pattern = "192.168.1.50"

# Standard TCP forwarding
[[forwarding_rules]]
bind_address = "0.0.0.0"
bind_port = 80
connect_address = "192.168.1.2"
connect_port = 80
protocol = "tcp"

# Standard UDP forwarding
[[forwarding_rules]]
bind_address = "0.0.0.0"
bind_port = 53
connect_address = "8.8.8.8"
connect_port = 53
protocol = "udp"
timeout = 1200

# UDP-to-TCP forwarding
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "udptotcp"

# TCP-to-UDP forwarding
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8081
connect_address = "127.0.0.1"
connect_port = 9091
protocol = "tcptoudp"
```

### Legacy .conf Format

You can also use the original rinetd format:

```conf
# Allow rules
allow 192.168.1.*

# Deny rules
deny 192.168.1.50

# Forwarding rules (bind_address bind_port connect_address connect_port)
0.0.0.0 80 192.168.1.2 80
0.0.0.0 53 8.8.8.8 53
```

## Protocol Options

- `tcp`: Standard TCP forwarding (default)
- `udp`: Standard UDP forwarding
- `udptotcp`: UDP-to-TCP cross-protocol forwarding
- `tcptoudp`: TCP-to-UDP cross-protocol forwarding

## Access Control

Access control rules can be defined globally or per forwarding rule:

```toml
# Global rules apply to all forwarding rules
[[global_rules]]
type = "allow"  # or "deny"
pattern = "192.168.1.*"  # IP pattern

# Per-rule rules apply only to specific forwarding rules
[[forwarding_rules]]
bind_address = "192.168.1.1"
bind_port = 8080
connect_address = "10.0.0.1"
connect_port = 80

# Access rules for this specific forwarding rule
[[forwarding_rules.rules]]
type = "allow"
pattern = "192.168.1.0/24"

[[forwarding_rules.rules]]
type = "deny"
pattern = "192.168.1.50"
```

## Testing

Run the test suite:

```bash
cargo test
```

See [TESTING.md](TESTING.md) for more details about the test suite.

## Building

### Development Build

```bash
cargo build
```

### Release Build

```bash
cargo build --release
```

## Contributing

1. Fork the repository  
2. Create branch  
3. Commit your changes  
4. Push to the branch  
5. Open a pull request

## License

This project is licensed under the GPL-2.0 License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Inspired by [rinetd](https://github.com/samhocevar/rinetd), a popular port forwarding utility
- Built with [Smol](https://github.com/smol-rs/smol), a simple async runtime for Rust
