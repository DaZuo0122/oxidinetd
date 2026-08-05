use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
    UdpToTcp,
    TcpToUdp,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ForwardingRule {
    pub bind_address: String,
    pub bind_port: u16,
    pub connect_address: String,
    pub connect_port: u16,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub source_address: Option<String>,
    #[serde(default)]
    pub rules: Vec<AccessRule>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AccessRule {
    #[serde(rename = "type")]
    pub rule_type: RuleType,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Rinetd,
    Common,
}

impl Default for LogFormat {
    fn default() -> Self {
        LogFormat::Rinetd
    }
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol::Tcp
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub global_rules: Vec<AccessRule>,
    pub forwarding_rules: Vec<ForwardingRule>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub pid_file: Option<String>,
    #[serde(default)]
    pub log_format: LogFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("tcp", Protocol::Tcp)]
    #[test_case("udp", Protocol::Udp)]
    #[test_case("udptotcp", Protocol::UdpToTcp)]
    #[test_case("tcptoudp", Protocol::TcpToUdp)]
    fn protocol_deser_lowercase(input: &str, expected: Protocol) {
        let rule: ForwardingRule =
            toml::from_str(&format!(r#"bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "{}""#, input))
                .unwrap();
        assert_eq!(rule.protocol, expected);
    }

    #[test]
    fn protocol_deser_invalid() {
        let err = toml::from_str::<ForwardingRule>(r#"bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "invalid""#);
        assert!(err.is_err());
    }

    #[test]
    fn protocol_default_is_tcp() {
        assert!(matches!(Protocol::default(), Protocol::Tcp));
    }

    #[test_case("allow", RuleType::Allow)]
    #[test_case("deny", RuleType::Deny)]
    fn rule_type_deser(input: &str, expected: RuleType) {
        let rule: AccessRule =
            toml::from_str(&format!(r#"type = "{}"
pattern = "192.168.1.*""#, input))
                .unwrap();
        assert_eq!(rule.rule_type, expected);
    }

    #[test_case("rinetd", LogFormat::Rinetd)]
    #[test_case("common", LogFormat::Common)]
    fn log_format_deser(input: &str, expected: LogFormat) {
        let config: Config =
            toml::from_str(&format!(r#"log_format = "{}"
forwarding_rules = []"#, input))
                .unwrap();
        assert_eq!(config.log_format, expected);
    }

    #[test]
    fn log_format_default_is_rinetd() {
        assert!(matches!(LogFormat::default(), LogFormat::Rinetd));
    }

    #[test]
    fn forwarding_rule_defaults() {
        let rule: ForwardingRule = toml::from_str(r#"bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090"#)
            .unwrap();
        assert!(matches!(rule.protocol, Protocol::Tcp));
        assert!(rule.timeout.is_none());
        assert!(rule.source_address.is_none());
        assert!(rule.rules.is_empty());
    }

    #[test]
    fn forwarding_rule_all_fields() {
        let rule: ForwardingRule = toml::from_str(r#"bind_address = "0.0.0.0"
bind_port = 53
connect_address = "8.8.8.8"
connect_port = 53
protocol = "udp"
timeout = 1200
source_address = "192.168.1.1"

[[rules]]
type = "allow"
pattern = "10.0.0.0/8"

[[rules]]
type = "deny"
pattern = "10.0.0.42""#)
            .unwrap();
        assert!(matches!(rule.protocol, Protocol::Udp));
        assert_eq!(rule.timeout, Some(1200));
        assert_eq!(rule.source_address.as_deref(), Some("192.168.1.1"));
        assert_eq!(rule.rules.len(), 2);
        assert!(matches!(rule.rules[0].rule_type, RuleType::Allow));
        assert_eq!(rule.rules[0].pattern, "10.0.0.0/8");
        assert!(matches!(rule.rules[1].rule_type, RuleType::Deny));
        assert_eq!(rule.rules[1].pattern, "10.0.0.42");
    }

    #[test]
    fn config_all_fields_present() {
        let config: Config = toml::from_str(r#"
log_file = "/var/log/oi.log"
pid_file = "/var/run/oi.pid"
log_format = "common"

[[global_rules]]
type = "allow"
pattern = "192.168.1.*"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
"#)
            .unwrap();
        assert_eq!(config.global_rules.len(), 1);
        assert_eq!(config.forwarding_rules.len(), 1);
        assert_eq!(config.log_file.as_deref(), Some("/var/log/oi.log"));
        assert_eq!(config.pid_file.as_deref(), Some("/var/run/oi.pid"));
        assert!(matches!(config.log_format, LogFormat::Common));
    }

    #[test]
    fn config_minimal_fields() {
        let config: Config = toml::from_str(r#"
[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
"#)
            .unwrap();
        assert!(config.global_rules.is_empty());
        assert_eq!(config.forwarding_rules.len(), 1);
        assert!(config.log_file.is_none());
        assert!(config.pid_file.is_none());
        assert!(matches!(config.log_format, LogFormat::Rinetd));
    }

    #[test]
    fn config_roundtrip_serialize() {
        let config: Config = toml::from_str(r#"
log_file = "/tmp/oi.log"
pid_file = "/tmp/oi.pid"
log_format = "rinetd"

[[global_rules]]
type = "deny"
pattern = "10.0.0.1"

[[forwarding_rules]]
bind_address = "127.0.0.1"
bind_port = 8080
connect_address = "127.0.0.1"
connect_port = 9090
protocol = "udptotcp"
timeout = 30
source_address = "127.0.0.2"

[[forwarding_rules.rules]]
type = "allow"
pattern = "127.0.0.*"
"#)
            .unwrap();

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(config.global_rules.len(), deserialized.global_rules.len());
        assert_eq!(config.forwarding_rules.len(), deserialized.forwarding_rules.len());
        assert_eq!(
            config.forwarding_rules[0].rules.len(),
            deserialized.forwarding_rules[0].rules.len()
        );
        assert_eq!(config.log_file, deserialized.log_file);
        assert_eq!(config.pid_file, deserialized.pid_file);
        assert_eq!(
            config.forwarding_rules[0].timeout,
            deserialized.forwarding_rules[0].timeout
        );
    }

    #[test]
    fn config_missing_forwarding_rules_errors() {
        let err = toml::from_str::<Config>("log_file = \"/tmp/oi.log\"");
        assert!(err.is_err());
    }

    #[test]
    fn access_rule_serde_rename_type() {
        let rule: AccessRule = toml::from_str(r#"type = "allow"
pattern = "10.0.0.1""#)
            .unwrap();
        assert!(matches!(rule.rule_type, RuleType::Allow));
        assert_eq!(rule.pattern, "10.0.0.1");
    }
}