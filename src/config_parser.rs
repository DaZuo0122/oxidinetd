use crate::config::{Config, ForwardingRule, AccessRule, RuleType, Protocol, LogFormat};
use std::fs;
use std::net::ToSocketAddrs;

#[derive(Debug)]
pub enum ConfigError {
    IoError(std::io::Error),
    ParseError(String),
    TomlError(toml::de::Error),
}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        ConfigError::IoError(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        ConfigError::TomlError(error)
    }
}

impl Config {
    pub fn load_from_file(path: &str) -> Result<Self, ConfigError> {
        if path.ends_with(".toml") {
            Self::parse_toml_config(path)
        } else {
            // Parse legacy .conf format
            Self::parse_legacy_conf(path)
        }
    }
    
    fn parse_toml_config(path: &str) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
    
    fn parse_legacy_conf(path: &str) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let mut global_rules = Vec::new();
        let mut forwarding_rules = Vec::new();
        
        for line in content.lines() {
            let line = line.trim();
            
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            
            let parts: Vec<&str> = line.split_whitespace().collect();
            
            // Check if this is a bind/connect rule (4 parts)
            if parts.len() == 4 {
                let bind_address = parts[0].to_string();
                let bind_port = parts[1].parse::<u16>()
                    .map_err(|_| ConfigError::ParseError(format!("Invalid bind port: {}", parts[1])))?;
                let connect_address = parts[2].to_string();
                let connect_port = parts[3].parse::<u16>()
                    .map_err(|_| ConfigError::ParseError(format!("Invalid connect port: {}", parts[3])))?;
                
                // Validate addresses
                let _ = (bind_address.as_str(), bind_port).to_socket_addrs()
                    .map_err(|_| ConfigError::ParseError(format!("Invalid bind address: {}:{}", bind_address, bind_port)))?;
                let _ = (connect_address.as_str(), connect_port).to_socket_addrs()
                    .map_err(|_| ConfigError::ParseError(format!("Invalid connect address: {}:{}", connect_address, connect_port)))?;
                
                forwarding_rules.push(ForwardingRule {
                    bind_address,
                    bind_port,
                    connect_address,
                    connect_port,
                    protocol: Protocol::Tcp, // Default to TCP
                    timeout: None,
                    source_address: None,
                    rules: Vec::new(),
                });
            }
            // Handle allow/deny rules (2 parts)
            else if parts.len() == 2 {
                let rule_type = match parts[0].to_lowercase().as_str() {
                    "allow" => RuleType::Allow,
                    "deny" => RuleType::Deny,
                    _ => return Err(ConfigError::ParseError(format!("Unknown rule type: {}", parts[0]))),
                };
                
                global_rules.push(AccessRule {
                    rule_type,
                    pattern: parts[1].to_string(),
                });
            }
        }
        
        Ok(Config {
            global_rules,
            forwarding_rules,
            log_file: None,
            pid_file: None,
            log_format: LogFormat::Rinetd,
        })
    }
}