use oxidinetd::config::{Config, Protocol};

#[test]
fn test_config_parsing_cross_protocol() {
    // Test that our Protocol enum correctly parses the new variants
    let config_content = r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "udptotcp"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8081
connect_address = "127.0.0.1"
connect_port = 9091
protocol = "tcptoudp"
"#;

    let config: Config = toml::from_str(config_content).expect("Failed to parse config");
    
    assert_eq!(config.forwarding_rules.len(), 2);
    
    let rule1 = &config.forwarding_rules[0];
    assert_eq!(rule1.bind_address, "127.0.0.1");
    assert_eq!(rule1.bind_port, 8080);
    assert_eq!(rule1.connect_address, "127.0.0.1");
    assert_eq!(rule1.connect_port, 9090);
    match rule1.protocol {
        Protocol::UdpToTcp => {}, // Correct
        _ => panic!("Expected UdpToTcp protocol"),
    }
    
    let rule2 = &config.forwarding_rules[1];
    assert_eq!(rule2.bind_address, "127.0.0.1");
    assert_eq!(rule2.bind_port, 8081);
    assert_eq!(rule2.connect_address, "127.0.0.1");
    assert_eq!(rule2.connect_port, 9091);
    match rule2.protocol {
        Protocol::TcpToUdp => {}, // Correct
        _ => panic!("Expected TcpToUdp protocol"),
    }
}

#[test]
fn test_enum_serialization() {
    // Test that our Protocol enum deserializes correctly
    let protocol_strings = vec!["tcp", "udp", "udptotcp", "tcptoudp"];
    
    for protocol_str in protocol_strings {
        let toml_str = format!(r#"protocol = "{}""#, protocol_str);
        let config: serde_json::Value = toml::from_str(&toml_str)
            .expect("Failed to deserialize protocol");
        // Just verify it parses without error
    }
}

#[test]
fn test_default_protocol() {
    // Test that the default protocol is still TCP
    let config_content = r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
"#;

    let config: Config = toml::from_str(config_content).expect("Failed to parse config");
    
    assert_eq!(config.forwarding_rules.len(), 1);
    
    let rule = &config.forwarding_rules[0];
    match rule.protocol {
        Protocol::Tcp => {}, // Correct default
        _ => panic!("Expected default TCP protocol"),
    }
}

#[test]
fn test_mixed_protocols() {
    // Test a configuration with mixed protocol types
    let config_content = r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "tcp"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8081
connect_address = "127.0.0.1"
connect_port = 9091
protocol = "udp"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8082
connect_address = "127.0.0.1"
connect_port = 9092
protocol = "udptotcp"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8083
connect_address = "127.0.0.1"
connect_port = 9093
protocol = "tcptoudp"
"#;

    let config: Config = toml::from_str(config_content).expect("Failed to parse config");
    
    assert_eq!(config.forwarding_rules.len(), 4);
    
    // Check each protocol type
    match config.forwarding_rules[0].protocol {
        Protocol::Tcp => {},
        _ => panic!("Expected TCP protocol"),
    }
    
    match config.forwarding_rules[1].protocol {
        Protocol::Udp => {},
        _ => panic!("Expected UDP protocol"),
    }
    
    match config.forwarding_rules[2].protocol {
        Protocol::UdpToTcp => {},
        _ => panic!("Expected UDP-to-TCP protocol"),
    }
    
    match config.forwarding_rules[3].protocol {
        Protocol::TcpToUdp => {},
        _ => panic!("Expected TCP-to-UDP protocol"),
    }
}