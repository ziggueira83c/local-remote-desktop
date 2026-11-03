//! Políticas de rede: somente redes locais (RN-004) e limitação de tentativas.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Aceita apenas endereços de rede local: RFC 1918, link-local, loopback, CGNAT (100.64/10,
/// usado por algumas VPNs domésticas) e IPv6 ULA/link-local.
pub fn is_allowed_peer_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_allowed_peer_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

/// Limita tentativas recusadas por IP: após `max_failures` em `window`, bloqueia por `lockout`.
pub struct RateLimiter {
    failures: HashMap<IpAddr, Vec<Instant>>,
    blocked_until: HashMap<IpAddr, Instant>,
    max_failures: usize,
    window: Duration,
    lockout: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(300), Duration::from_secs(300))
    }
}

impl RateLimiter {
    pub fn new(max_failures: usize, window: Duration, lockout: Duration) -> Self {
        RateLimiter { failures: HashMap::new(), blocked_until: HashMap::new(), max_failures, window, lockout }
    }

    pub fn is_blocked(&mut self, ip: IpAddr) -> bool {
        match self.blocked_until.get(&ip) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                self.blocked_until.remove(&ip);
                false
            }
            None => false,
        }
    }

    pub fn record_failure(&mut self, ip: IpAddr) {
        let now = Instant::now();
        let window = self.window;
        let list = self.failures.entry(ip).or_default();
        list.retain(|t| now.duration_since(*t) < window);
        list.push(now);
        if list.len() >= self.max_failures {
            self.blocked_until.insert(ip, now + self.lockout);
            list.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        assert!(is_allowed_peer_ip("192.168.1.10".parse().unwrap()));
        assert!(is_allowed_peer_ip("10.0.0.5".parse().unwrap()));
        assert!(is_allowed_peer_ip("fe80::1".parse().unwrap()));
        assert!(!is_allowed_peer_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_allowed_peer_ip("2001:4860::1".parse().unwrap()));
    }

    #[test]
    fn rate_limit_blocks() {
        let mut r = RateLimiter::new(2, Duration::from_secs(60), Duration::from_secs(60));
        let ip: IpAddr = "192.168.0.2".parse().unwrap();
        r.record_failure(ip);
        assert!(!r.is_blocked(ip));
        r.record_failure(ip);
        assert!(r.is_blocked(ip));
    }
}
