//! Sibling-host resolution — a port of `src/condense/hosts.py`.
//!
//! Strips a known-service subdomain prefix to find the deployment zone,
//! then re-prefixes to address a sibling service. Keeps the binary's view
//! of api/login/cli hosts identical to the server's.

pub const KNOWN_SERVICES: &[&str] = &["api", "admin", "cli", "helm", "login", "landing"];

/// Local-dev hosts default to http; everything else to https.
pub fn default_scheme_for(host: &str) -> &'static str {
    if is_dev_host(host) { "http" } else { "https" }
}

/// Strip scheme + path from a URL, leaving `host[:port]`.
pub fn host_of(url: &str) -> String {
    let after = url.split_once("://").map_or(url, |(_, rest)| rest);
    after.split('/').next().unwrap_or(after).to_string()
}

/// `true` for `*.localhost` / `*.test` zones (local dev).
pub fn is_dev_host(host: &str) -> bool {
    let (h, _) = strip_port(host);
    let h = h.to_ascii_lowercase();
    h.ends_with(".localhost") || h.ends_with(".test")
}

/// URL of `service` on the same zone as `host`, preserving any port.
pub fn sibling(host: &str, service: &str, scheme: &str) -> String {
    let (_, port) = strip_port(host);
    let port = if port.is_empty() {
        String::new()
    } else {
        format!(":{port}")
    };
    format!("{scheme}://{service}.{}{port}", zone_of(host))
}

/// Strip a leading known-service label; return the remaining zone. If the
/// leftmost label is not a known service, the whole host is the zone.
pub fn zone_of(host: &str) -> String {
    let (h, _) = strip_port(host);
    match h.split_once('.') {
        Some((first, rest)) if KNOWN_SERVICES.contains(&first.to_ascii_lowercase().as_str()) => {
            rest.to_string()
        }
        _ => h.to_string(),
    }
}

fn strip_port(host: &str) -> (&str, &str) {
    match host.split_once(':') {
        Some((h, p)) => (h, p),
        None => (host, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_host_detection() {
        assert!(is_dev_host("api.dev.condense.localhost"));
        assert!(is_dev_host("api.dev.condense.test:8080"));
        assert!(!is_dev_host("api.condense.chat"));
    }

    #[test]
    fn sibling_addresses_zone() {
        assert_eq!(
            sibling("cli.condense.chat", "api", "https"),
            "https://api.condense.chat"
        );
        assert_eq!(
            sibling("cli.dev.condense.localhost", "login", "http"),
            "http://login.dev.condense.localhost"
        );
    }

    #[test]
    fn zone_keeps_unknown_prefix() {
        assert_eq!(zone_of("condense.chat"), "condense.chat");
        assert_eq!(zone_of("foo.example.com"), "foo.example.com");
    }

    #[test]
    fn zone_strips_known_service() {
        assert_eq!(zone_of("cli.condense.chat"), "condense.chat");
        assert_eq!(zone_of("api.ts.condense.chat"), "ts.condense.chat");
        assert_eq!(
            zone_of("cli.dev.condense.localhost:8080"),
            "dev.condense.localhost"
        );
    }
}
