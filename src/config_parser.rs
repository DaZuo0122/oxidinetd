use crate::config::{Config, ForwardingRule, AccessRule, RuleType, Protocol, LogFormat};
use std::fmt;
use std::fs;
use std::net::ToSocketAddrs;

#[derive(Debug)]
pub enum ConfigError {
    IoError(std::io::Error),
    ParseError(String),
    TomlError(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::IoError(e) => write!(f, "I/O error: {}", e),
            ConfigError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            ConfigError::TomlError(e) => write!(f, "TOML error: {}", e),
        }
    }
}

impl std::error::Error for ConfigError {}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuleType;
    use std::io::Write;

    fn write_temp_file(content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.conf");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        (dir, path.to_str().unwrap().to_string())
    }

    #[test]
    fn config_error_display_io() {
        let err = ConfigError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert!(err.to_string().contains("I/O error"));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn config_error_display_parse() {
        let err = ConfigError::ParseError("bad line".to_string());
        assert!(err.to_string().contains("Parse error"));
        assert!(err.to_string().contains("bad line"));
    }

    #[test]
    fn config_error_display_toml() {
        let toml_err = toml::from_str::<Config>("not = [valid").unwrap_err();
        let err = ConfigError::TomlError(toml_err);
        assert!(err.to_string().contains("TOML error"));
    }

    #[test]
    fn config_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let err: ConfigError = io_err.into();
        assert!(matches!(err, ConfigError::IoError(_)));
    }

    #[test]
    fn config_error_from_toml_error() {
        let toml_err = toml::from_str::<Config>("[[[broken").unwrap_err();
        let err: ConfigError = toml_err.into();
        assert!(matches!(err, ConfigError::TomlError(_)));
    }

    #[test]
    fn load_from_file_dispatches_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.toml");
        fs::write(&path, r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
"#)
        .unwrap();

        let config = Config::load_from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.forwarding_rules.len(), 1);
        assert!(matches!(config.forwarding_rules[0].protocol, Protocol::Tcp));
    }

    #[test]
    fn load_from_file_dispatches_legacy_conf() {
        let (_dir, path) = write_temp_file("127.0.0.1 80 127.0.0.1 8080\n");
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.forwarding_rules.len(), 1);
    }

    #[test]
    fn load_from_file_file_not_found() {
        let err = Config::load_from_file("C:/nonexistent/dir/proxy.toml").unwrap_err();
        assert!(matches!(err, ConfigError::IoError(_)));
    }

    #[test]
    fn parse_toml_invalid_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("bad.toml");
        fs::write(&toml_path, "this is not valid toml ===").unwrap();

        let err = Config::load_from_file(toml_path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ConfigError::TomlError(_)));
    }

    #[test]
    fn parse_toml_invalid_port_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = "eight-zero-eight-zero"
connect_address = "127.0.0.1"
connect_port = 9090
"#)
        .unwrap();
        let err = Config::load_from_file(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ConfigError::TomlError(_)));
    }

    #[test]
    fn parse_legacy_simple_rule() {
        let (_dir, path) = write_temp_file("0.0.0.0 80 192.168.1.2 8080\n");
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.forwarding_rules.len(), 1);
        let rule = &config.forwarding_rules[0];
        assert_eq!(rule.bind_address, "0.0.0.0");
        assert_eq!(rule.bind_port, 80);
        assert_eq!(rule.connect_address, "192.168.1.2");
        assert_eq!(rule.connect_port, 8080);
        assert!(matches!(rule.protocol, Protocol::Tcp));
        assert!(rule.timeout.is_none());
        assert!(rule.source_address.is_none());
        assert!(rule.rules.is_empty());
        assert!(config.global_rules.is_empty());
        assert!(config.log_file.is_none());
        assert!(config.pid_file.is_none());
        assert!(matches!(config.log_format, LogFormat::Rinetd));
    }

    #[test]
    fn parse_legacy_multiple_rules() {
        let (_dir, path) = write_temp_file(
            "0.0.0.0 80 192.168.1.2 8080\n127.0.0.1 443 10.0.0.1 8443\n",
        );
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.forwarding_rules.len(), 2);
        assert_eq!(config.forwarding_rules[1].bind_port, 443);
        assert_eq!(config.forwarding_rules[1].connect_port, 8443);
    }

    #[test]
    fn parse_legacy_allow_rule() {
        let (_dir, path) = write_temp_file("allow 192.168.1.*\n");
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.global_rules.len(), 1);
        assert!(matches!(config.global_rules[0].rule_type, RuleType::Allow));
        assert_eq!(config.global_rules[0].pattern, "192.168.1.*");
        assert!(config.forwarding_rules.is_empty());
    }

    #[test]
    fn parse_legacy_deny_rule() {
        let (_dir, path) = write_temp_file("deny 10.0.0.1\n");
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.global_rules.len(), 1);
        assert!(matches!(config.global_rules[0].rule_type, RuleType::Deny));
    }

    #[test]
    fn parse_legacy_deny_uppercase() {
        let (_dir, path) = write_temp_file("DENY 10.0.0.1\n");
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.global_rules.len(), 1);
        assert!(matches!(config.global_rules[0].rule_type, RuleType::Deny));
    }

    #[test]
    fn parse_legacy_comment_and_empty_lines() {
        let (_dir, path) = write_temp_file(
            "# This is a comment\n\n   \n127.0.0.1 80 127.0.0.1 8080\n",
        );
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.forwarding_rules.len(), 1);
        assert!(config.global_rules.is_empty());
    }

    #[test]
    fn parse_legacy_invalid_rule_too_few_fields() {
        // Lines with a token count other than 2 or 4 are silently skipped
        let (_dir, path) = write_temp_file("0.0.0.0 80 192.168.1.2\n");
        let config = Config::load_from_file(&path).unwrap();
        assert!(config.forwarding_rules.is_empty());
    }

    #[test]
    fn parse_legacy_unknown_rule_type() {
        let (_dir, path) = write_temp_file("forward 192.168.1.2\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Unknown rule type: forward")));
    }

    #[test]
    fn parse_legacy_invalid_port() {
        let (_dir, path) = write_temp_file("0.0.0.0 abc 192.168.1.2 8080\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Invalid bind port")));
    }

    #[test]
    fn parse_legacy_invalid_connect_port() {
        let (_dir, path) = write_temp_file("0.0.0.0 80 192.168.1.2 xyz\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Invalid connect port")));
    }

    #[test]
    fn parse_legacy_port_out_of_range() {
        let (_dir, path) = write_temp_file("0.0.0.0 70000 192.168.1.2 8080\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Invalid bind port")));
    }

    #[test]
    fn parse_legacy_unresolvable_bind_addr() {
        let (_dir, path) = write_temp_file("invalidhost.invalid 80 127.0.0.1 8080\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Invalid bind address")));
    }

    #[test]
    fn parse_legacy_unresolvable_connect_addr() {
        let (_dir, path) = write_temp_file("127.0.0.1 80 invalidhost.invalid 8080\n");
        let err = Config::load_from_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(msg) if msg.contains("Invalid connect address")));
    }

    #[test]
    fn parse_legacy_file_not_found() {
        let err = Config::load_from_file("C:/nonexistent/dir/proxy.conf").unwrap_err();
        assert!(matches!(err, ConfigError::IoError(_)));
    }

    #[test]
    fn parse_legacy_mixed_access_and_rules() {
        let (_dir, path) = write_temp_file(
            "allow 192.168.1.*\ndeny 10.0.0.1\n127.0.0.1 80 127.0.0.1 8080\n",
        );
        let config = Config::load_from_file(&path).unwrap();
        assert_eq!(config.global_rules.len(), 2);
        assert_eq!(config.forwarding_rules.len(), 1);
    }

    #[test]
    fn parse_legacy_single_token_line() {
        // Lines with a token count other than 2 or 4 are silently skipped
        let (_dir, path) = write_temp_file("onlyonetoken\n");
        let config = Config::load_from_file(&path).unwrap();
        assert!(config.forwarding_rules.is_empty());
        assert!(config.global_rules.is_empty());
    }

    #[test]
    fn parse_legacy_five_token_line_is_not_a_rule() {
        // Lines with a token count other than 2 or 4 are silently skipped
        let (_dir, path) = write_temp_file("127.0.0.1 80 127.0.0.1 8080 extra\n");
        let config = Config::load_from_file(&path).unwrap();
        assert!(config.forwarding_rules.is_empty());
    }
}