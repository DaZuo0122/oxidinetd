use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
    UdpToTcp,
    TcpToUdp,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccessRule {
    #[serde(rename = "type")]
    pub rule_type: RuleType,
    pub pattern: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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