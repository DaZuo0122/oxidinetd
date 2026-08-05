use clap::Parser;
use oxidinetd::config::{Config, Protocol};

#[derive(Parser)]
#[clap(name = "oxidinted", version = "0.1.0")]
struct Args {
    /// Configuration file path
    #[clap(short, long)]
    config: String,

    /// Verbose mode
    #[clap(short, long)]
    verbose: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.verbose {
        println!("Loading configuration from {}", args.config);
    }

    // Load configuration
    let config = match Config::load_from_file(&args.config) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error loading config: {:?}", e);
            std::process::exit(1);
        }
    };

    println!("Loaded {} forwarding rules", config.forwarding_rules.len());

    // Run the async runtime
    match smol::block_on(async {
        // Create a vector to hold our tasks
        let mut tasks = Vec::new();

        // Set up signal handler for graceful shutdown
        let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
        let shutdown_tx_clone = shutdown_tx.clone();

        ctrlc::set_handler(move || {
            println!("Received Ctrl+C, shutting down...");
            let _ = shutdown_tx_clone.try_send(());
        })
        .expect("Error setting Ctrl+C handler");

        // Start all forwarding rules
        for rule in &config.forwarding_rules {
            let bind_addr = format!("{}:{}", rule.bind_address, rule.bind_port);
            let connect_addr = format!("{}:{}", rule.connect_address, rule.connect_port);

            // Resolve bind address
            let bind_socket_addr = match bind_addr.parse::<std::net::SocketAddr>() {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("Error parsing bind address {}: {}", bind_addr, e);
                    continue;
                }
            };

            match rule.protocol {
                Protocol::Tcp | Protocol::TcpToUdp => {
                    println!(
                        "Starting TCP forwarding from {} to {}",
                        bind_addr, connect_addr
                    );
                    let connect_addr_clone = connect_addr.clone();
                    let protocol_clone = rule.protocol.clone();

                    let task = smol::spawn(async move {
                        if let Err(e) =
                            oxidinetd::tcp_handler::start_tcp_forwarding(bind_socket_addr, connect_addr_clone, protocol_clone)
                                .await
                        {
                            eprintln!("TCP forwarding error: {}", e);
                        }
                    });

                    tasks.push(task);
                }
                Protocol::Udp | Protocol::UdpToTcp => {
                    println!(
                        "Starting UDP forwarding from {} to {}",
                        bind_addr, connect_addr
                    );
                    let connect_addr_clone = connect_addr.clone();
                    let timeout = rule.timeout;
                    let protocol_clone = rule.protocol.clone();

                    let task = smol::spawn(async move {
                        if let Err(e) =
                            oxidinetd::udp_handler::start_udp_forwarding(bind_socket_addr, connect_addr_clone, timeout, protocol_clone)
                                .await
                        {
                            eprintln!("UDP forwarding error: {}", e);
                        }
                    });

                    tasks.push(task);
                }
            }
        }

        // Wait for shutdown signal
        let _ = shutdown_rx.recv().await;
        println!("Shutting down...");

        Ok::<(), Box<dyn std::error::Error>>(())
    }) {
        Ok(_) => println!("Server shut down successfully"),
        Err(e) => {
            eprintln!("Server error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn args_parse_config_short() {
        let args = Args::parse_from(["oi", "-c", "proxy.toml"]);
        assert_eq!(args.config, "proxy.toml");
        assert!(!args.verbose);
    }

    #[test]
    fn args_parse_config_long() {
        let args = Args::parse_from(["oi", "--config", "proxy.toml"]);
        assert_eq!(args.config, "proxy.toml");
        assert!(!args.verbose);
    }

    #[test]
    fn args_parse_verbose_short() {
        let args = Args::parse_from(["oi", "-c", "proxy.toml", "-v"]);
        assert!(args.verbose);
    }

    #[test]
    fn args_parse_verbose_long() {
        let args = Args::parse_from(["oi", "-c", "proxy.toml", "--verbose"]);
        assert!(args.verbose);
    }

    #[test]
    fn args_parse_combined() {
        let args = Args::parse_from(["oi", "-c", "proxy.toml", "-v"]);
        assert_eq!(args.config, "proxy.toml");
        assert!(args.verbose);
    }

    #[test]
    fn args_parse_verbose_without_config_errors() {
        let err = Args::try_parse_from(["oi", "-v"]);
        assert!(err.is_err());
    }

    #[test]
    fn args_parse_missing_config_errors() {
        let err = Args::try_parse_from(["oi"]);
        assert!(err.is_err());
    }

    #[test]
    fn args_parse_unknown_flag_errors() {
        let err = Args::try_parse_from(["oi", "-c", "proxy.toml", "--bogus"]);
        assert!(err.is_err());
    }
}
