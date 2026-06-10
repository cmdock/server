use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use async_trait::async_trait;

use crate::app_state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWebhookTarget {
    pub url: String,
    pub host: String,
    pub resolved_addrs: Vec<SocketAddr>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebhookTargetError {
    #[error("webhook URL must be valid HTTPS")]
    InvalidUrl,
    #[error("webhook URL resolves to a private or local address")]
    PrivateAddress,
    #[error("webhook URL host could not be resolved")]
    UnresolvableHost,
}

#[async_trait]
pub trait WebhookDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str) -> anyhow::Result<Vec<IpAddr>>;
}

#[derive(Debug, Default)]
pub struct SystemWebhookDnsResolver;

#[async_trait]
impl WebhookDnsResolver for SystemWebhookDnsResolver {
    async fn resolve(&self, host: &str) -> anyhow::Result<Vec<IpAddr>> {
        let resolved = tokio::net::lookup_host((host, 443)).await?;
        let ips = resolved.map(|addr| addr.ip()).collect();
        Ok(ips)
    }
}

pub async fn prepare_target(
    state: &AppState,
    raw: &str,
) -> Result<PreparedWebhookTarget, WebhookTargetError> {
    let parsed = reqwest::Url::parse(raw.trim()).map_err(|_| WebhookTargetError::InvalidUrl)?;
    if parsed.scheme() != "https" {
        return Err(WebhookTargetError::InvalidUrl);
    }

    let host = parsed
        .host_str()
        .ok_or(WebhookTargetError::InvalidUrl)?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);

    let resolved_ips = if let Ok(ip) = IpAddr::from_str(&host) {
        vec![ip]
    } else {
        let resolved = state
            .webhook_dns_resolver
            .resolve(&host)
            .await
            .map_err(|_| WebhookTargetError::UnresolvableHost)?;
        if resolved.is_empty() {
            return Err(WebhookTargetError::UnresolvableHost);
        }
        resolved
    };

    if resolved_ips.iter().any(|ip| is_forbidden_ip(*ip)) {
        return Err(WebhookTargetError::PrivateAddress);
    }

    let mut resolved_addrs = Vec::new();
    let mut seen = BTreeSet::new();
    for ip in resolved_ips {
        let addr = SocketAddr::new(ip, port);
        if seen.insert(addr) {
            resolved_addrs.push(addr);
        }
    }

    Ok(PreparedWebhookTarget {
        url: parsed.to_string(),
        host,
        resolved_addrs,
    })
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    use super::is_forbidden_ip;

    #[test]
    fn rejects_private_and_local_ipv4_ranges() {
        assert!(is_forbidden_ip(Ipv4Addr::new(10, 0, 0, 1).into()));
        assert!(is_forbidden_ip(Ipv4Addr::new(127, 0, 0, 1).into()));
        assert!(is_forbidden_ip(Ipv4Addr::new(169, 254, 1, 1).into()));
        assert!(is_forbidden_ip(Ipv4Addr::UNSPECIFIED.into()));
        assert!(!is_forbidden_ip(Ipv4Addr::new(93, 184, 216, 34).into()));
    }

    #[test]
    fn rejects_private_and_local_ipv6_ranges() {
        assert!(is_forbidden_ip(Ipv6Addr::LOCALHOST.into()));
        assert!(is_forbidden_ip(Ipv6Addr::UNSPECIFIED.into()));
        assert!(is_forbidden_ip(
            Ipv6Addr::from_str("fc00::1").unwrap().into()
        ));
        assert!(is_forbidden_ip(
            Ipv6Addr::from_str("fe80::1").unwrap().into()
        ));
        assert!(!is_forbidden_ip(
            Ipv6Addr::from_str("2606:2800:220:1:248:1893:25c8:1946")
                .unwrap()
                .into()
        ));
    }
}
