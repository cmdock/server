//! Structured audit logging layer.
//!
//! Provides a separate tracing subscriber layer that filters `target == "audit"`
//! events and routes them to configured output (stderr JSON or file). When audit
//! is disabled, the layer is not registered and events are dropped at near-zero cost.
//!
//! Output modes:
//! - `"stderr"` (default): audit JSON goes to stderr, app logs go to stdout
//! - `"stdout"` (legacy alias): same as "stderr"
//! - Any other value: treated as a file path, opened in append mode with 0600 perms

use std::fs::OpenOptions;
use std::net::IpAddr;
use std::sync::Mutex;

use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt;
use tracing_subscriber::Layer;

use crate::config::AuditSection;

/// Build the audit subscriber layer.
///
/// Returns `Ok(None)` if audit is disabled, `Ok(Some(layer))` on success,
/// or `Err` if audit is enabled but the output cannot be opened.
pub fn setup_audit_layer<S>(
    config: &AuditSection,
) -> anyhow::Result<Option<Box<dyn Layer<S> + Send + Sync>>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if !config.enabled {
        return Ok(None);
    }

    let filter = Targets::new().with_target("audit", Level::TRACE);

    if config.output == "stderr" || config.output == "stdout" {
        // Write audit JSON to stderr — separate from app fmt logs on stdout
        let layer = fmt::layer()
            .json()
            .with_target(true)
            .with_writer(std::io::stderr)
            .with_filter(filter);
        Ok(Some(Box::new(layer)))
    } else {
        let file = {
            let mut opts = OpenOptions::new();
            opts.create(true).append(true);

            // Set restrictive permissions on Unix (0600 — owner read/write only)
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }

            opts.open(&config.output).map_err(|e| {
                eprintln!(
                    "FATAL: Failed to open audit log file '{}': {e}",
                    config.output
                );
                anyhow::anyhow!("Failed to open audit log file '{}': {e}", config.output)
            })?
        };

        // Ensure restrictive permissions even on pre-existing files
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                eprintln!("WARNING: Failed to set audit log permissions to 0600: {e}");
            }
        }
        // Use LineWriter for durability — flushes on each newline so audit
        // records survive crashes (BufWriter would hold records in userspace buffer)
        let writer = Mutex::new(std::io::LineWriter::new(file));
        let layer = fmt::layer()
            .json()
            .with_target(true)
            .with_writer(writer)
            .with_filter(filter);
        Ok(Some(Box::new(layer)))
    }
}

/// Extract the client IP from trusted forwarded headers or return "unknown".
pub fn client_ip(headers: &axum::http::HeaderMap, trust_forwarded_headers: bool) -> String {
    if !trust_forwarded_headers {
        return "unknown".to_string();
    }
    // Check X-Forwarded-For first (reverse proxy)
    if let Some(xff) = headers.get("x-forwarded-for") {
        if let Ok(s) = xff.to_str() {
            for candidate in s.split(',') {
                if let Some(ip) = parse_ip(candidate) {
                    return ip;
                }
            }
        }
    }
    // Check X-Real-Ip
    if let Some(xri) = headers.get("x-real-ip") {
        if let Ok(s) = xri.to_str() {
            if let Some(ip) = parse_ip(s) {
                return ip;
            }
        }
    }
    "unknown".to_string()
}

pub fn request_id(headers: &axum::http::HeaderMap) -> Option<String> {
    header_value(headers, "x-request-id")
}

pub fn session_id(headers: &axum::http::HeaderMap) -> Option<String> {
    header_value(headers, "x-session-id")
}

pub fn mutation_id(headers: &axum::http::HeaderMap) -> Option<String> {
    header_value(headers, "x-mutation-id")
}

fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_ip(raw: &str) -> Option<String> {
    raw.trim().parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn test_client_ip_xff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "10.0.0.1");
    }

    #[test]
    fn test_client_ip_xff_multiple() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "1.2.3.4");
    }

    #[test]
    fn test_client_ip_xri() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "192.168.1.1".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "192.168.1.1");
    }

    #[test]
    fn test_client_ip_xff_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
        headers.insert("x-real-ip", "192.168.1.1".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "10.0.0.1");
    }

    #[test]
    fn test_client_ip_none() {
        let headers = HeaderMap::new();
        assert_eq!(client_ip(&headers, true), "unknown");
    }

    #[test]
    fn test_client_ip_ignores_malformed_xff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "unknown");
    }

    #[test]
    fn test_client_ip_uses_first_valid_xff_entry() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "garbage, 203.0.113.42, 198.51.100.4".parse().unwrap(),
        );
        assert_eq!(client_ip(&headers, true), "203.0.113.42");
    }

    #[test]
    fn test_client_ip_falls_back_to_valid_xri() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "garbage".parse().unwrap());
        headers.insert("x-real-ip", "198.51.100.8".parse().unwrap());
        assert_eq!(client_ip(&headers, true), "198.51.100.8");
    }

    #[test]
    fn test_client_ip_ignores_forwarded_headers_when_not_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.42".parse().unwrap());
        headers.insert("x-real-ip", "198.51.100.8".parse().unwrap());
        assert_eq!(client_ip(&headers, false), "unknown");
    }

    #[test]
    fn test_request_id_parsed_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "req-123".parse().unwrap());
        assert_eq!(request_id(&headers).as_deref(), Some("req-123"));
    }

    #[test]
    fn test_session_and_mutation_ids_trim_and_ignore_empty() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", " sess-1 ".parse().unwrap());
        headers.insert("x-mutation-id", "   ".parse().unwrap());
        assert_eq!(session_id(&headers).as_deref(), Some("sess-1"));
        assert_eq!(mutation_id(&headers), None);
    }
}
