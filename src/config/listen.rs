//! Listener address validation, matching Go's `validateListen`.

use std::net::IpAddr;

/// Accept an empty host, an IPv4/IPv6 literal (bracketed IPv6 may carry a
/// zone), or a hostname, plus a numeric port from 0 through 65535. Named
/// service ports are rejected.
pub fn valid(address: &str) -> bool {
    if address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((host, port)) = split_host_port(address) else {
        return false;
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if port.parse::<u16>().is_err() {
        return false;
    }
    let bracketed = address.starts_with('[');
    if bracketed || host.contains(':') {
        let (ip, zone) = match host.split_once('%') {
            Some((ip, zone)) => (ip, Some(zone)),
            None => (host, None),
        };
        if zone.is_some_and(str::is_empty) {
            return false;
        }
        return ip.parse::<IpAddr>().is_ok() && ip.contains(':');
    }
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        return true;
    }
    valid_hostname(host)
}

fn split_host_port(address: &str) -> Option<(&str, &str)> {
    if let Some(rest) = address.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = rest[end + 1..].strip_prefix(':')?;
        if port.contains(':') {
            return None;
        }
        return Some((host, port));
    }
    let index = address.rfind(':')?;
    let (host, port) = address.split_at(index);
    if host.contains(':') {
        return None;
    }
    Some((host, &port[1..]))
}

fn valid_hostname(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_documented_forms() {
        for address in [
            ":8080",
            "localhost:8080",
            "127.0.0.1:8080",
            "[::1]:8080",
            "[fe80::1%eth0]:8080",
            "example.com:0",
            "example.com:65535",
        ] {
            assert!(valid(address), "{address}");
        }
    }

    #[test]
    fn rejects_service_ports_and_bad_hosts() {
        for address in [
            "localhost:http",
            "localhost:",
            "localhost",
            "127.0.0.1:65536",
            "::1:8080",
            "bad_host:80",
            "-bad.example:80",
            "bad-.example:80",
            "[::1]",
            "[::1]:80x",
            "host: 80",
        ] {
            assert!(!valid(address), "{address}");
        }
    }
}
