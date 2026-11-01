//! Bare HTTP(S) origin validation, matching Go's `parseOrigin`/`originAddr`.

use std::fmt;

use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidOrigin;

impl fmt::Display for InvalidOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a bare http(s) origin")
    }
}

impl std::error::Error for InvalidOrigin {}

/// Validate and trim a bare origin. Returns the trimmed original spelling so
/// the configured authority is preserved for forwarding.
pub fn parse_origin(raw: &str) -> Result<String, InvalidOrigin> {
    let base = raw.trim().trim_end_matches('/');
    let url = Url::parse(base).map_err(|_| InvalidOrigin)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(InvalidOrigin);
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(InvalidOrigin);
    }
    // A host-only URL always parses with path "/". Anything beyond that is a
    // configured path, which must live in the rewrite rules instead.
    if url.path() != "/" {
        return Err(InvalidOrigin);
    }
    if url.query().is_some() || url.fragment().is_some() || base.contains('#') {
        return Err(InvalidOrigin);
    }
    Ok(base.to_string())
}

/// Canonicalize an origin to `host:port` for peer/plaintext collision checks.
pub fn origin_addr(base: &str) -> Option<String> {
    let url = Url::parse(base).ok()?;
    let host = match url.host()? {
        Host::Domain(domain) => domain,
        Host::Ipv4(ip) => return Some(format!("{ip}:{}", url.port_or_known_default()?)),
        Host::Ipv6(ip) => return Some(format!("{ip}:{}", url.port_or_known_default()?)),
    };
    Some(format!("{host}:{}", url.port_or_known_default()?))
}

/// The `host:port` the proxy uses to look up an encrypted peer. Default ports
/// are made explicit so config spellings agree with outbound request URIs.
pub fn authority_key(base: &str) -> Option<String> {
    origin_addr(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bare_origins_and_trims_slashes() {
        for raw in [
            "https://api.openai.com",
            "https://api.openai.com/",
            "http://127.0.0.1:11434",
            "http://[::1]:8080",
            "  https://example.com  ",
        ] {
            assert!(parse_origin(raw).is_ok(), "{raw}");
        }
        assert_eq!(
            parse_origin("https://example.com/").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn rejects_paths_queries_fragments_and_credentials() {
        for raw in [
            "https://example.com/v1",
            "https://example.com?x=1",
            "https://example.com?",
            "https://example.com#frag",
            "https://user:pass@example.com",
            "ftp://example.com",
            "not a url",
        ] {
            assert!(parse_origin(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn canonicalizes_ports() {
        assert_eq!(
            origin_addr("https://example.com").unwrap(),
            "example.com:443"
        );
        assert_eq!(origin_addr("http://example.com").unwrap(), "example.com:80");
        assert_eq!(
            origin_addr("http://127.0.0.1:11434").unwrap(),
            "127.0.0.1:11434"
        );
    }
}
