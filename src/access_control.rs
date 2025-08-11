use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone)]
pub struct IpPattern {
    pub pattern: String,
}

impl IpPattern {
    pub fn matches(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ipv4) => self.matches_ipv4(ipv4),
            IpAddr::V6(ipv6) => self.matches_ipv6(ipv6),
        }
    }
    
    fn matches_ipv4(&self, ip: Ipv4Addr) -> bool {
        // Handle wildcard patterns like "192.168.1.*"
        if self.pattern.contains('*') {
            let pattern_parts: Vec<&str> = self.pattern.split('.').collect();
            let ip_parts: Vec<u8> = ip.octets().to_vec();
            
            if pattern_parts.len() != 4 {
                return false;
            }
            
            for i in 0..4 {
                if pattern_parts[i] == "*" {
                    continue;
                }
                
                if let Ok(part) = pattern_parts[i].parse::<u8>() {
                    if part != ip_parts[i] {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            
            return true;
        }
        
        // Handle CIDR notation like "192.168.1.0/24"
        if self.pattern.contains('/') {
            let parts: Vec<&str> = self.pattern.split('/').collect();
            if parts.len() != 2 {
                return false;
            }
            
            if let Ok(prefix_len) = parts[1].parse::<u8>() {
                if let Ok(pattern_ip) = parts[0].parse::<Ipv4Addr>() {
                    return Self::ipv4_matches_cidr(ip, pattern_ip, prefix_len);
                }
            }
            
            return false;
        }
        
        // Handle exact match
        if let Ok(pattern_ip) = self.pattern.parse::<Ipv4Addr>() {
            return ip == pattern_ip;
        }
        
        false
    }
    
    fn matches_ipv6(&self, ip: Ipv6Addr) -> bool {
        // For IPv6, we'll just do exact matching for now
        if let Ok(pattern_ip) = self.pattern.parse::<Ipv6Addr>() {
            return ip == pattern_ip;
        }
        
        false
    }
    
    fn ipv4_matches_cidr(ip: Ipv4Addr, pattern_ip: Ipv4Addr, prefix_len: u8) -> bool {
        if prefix_len > 32 {
            return false;
        }
        
        let ip_bits = u32::from_be_bytes(ip.octets());
        let pattern_bits = u32::from_be_bytes(pattern_ip.octets());
        
        let mask = !0u32 << (32 - prefix_len);
        
        (ip_bits & mask) == (pattern_bits & mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    
    #[test]
    fn test_ipv4_wildcard_matching() {
        let pattern = IpPattern { pattern: "192.168.1.*".to_string() };
        
        assert!(pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255))));
        assert!(!pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 10))));
    }
    
    #[test]
    fn test_ipv4_exact_matching() {
        let pattern = IpPattern { pattern: "192.168.1.10".to_string() };
        
        assert!(pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(!pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 11))));
    }
    
    #[test]
    fn test_ipv4_cidr_matching() {
        let pattern = IpPattern { pattern: "192.168.1.0/24".to_string() };
        
        assert!(pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255))));
        assert!(!pattern.matches(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 10))));
    }
    
    #[test]
    fn test_ipv6_exact_matching() {
        let pattern = IpPattern { pattern: "2001:db8::1".to_string() };
        let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        
        assert!(pattern.matches(ip));
    }
}