mod access_control;
mod config;
mod config_parser;
mod tcp_handler;
mod udp_handler;

use crate::config::{Config, Protocol};
use clap::Parser;

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
                            tcp_handler::start_tcp_forwarding(bind_socket_addr, connect_addr_clone, protocol_clone)
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
                            udp_handler::start_udp_forwarding(bind_socket_addr, connect_addr_clone, timeout, protocol_clone)
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
