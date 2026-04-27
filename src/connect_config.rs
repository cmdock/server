use anyhow::Context;
use base64::Engine;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::{Deserialize, Serialize};

pub const CONNECT_CONFIG_VERSION: u8 = 1;
pub const CONNECT_CONFIG_TYPE: &str = "connect";
pub const MAX_CONNECT_URL_BYTES: usize = 300;
pub const DEFAULT_CONNECT_TOKEN_BYTES: usize = 18;
pub const DEFAULT_CONNECT_TOKEN_TTL_MINUTES: i64 = 60;
pub const MAX_CONNECT_TOKEN_ID_BYTES: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectConfigPayload {
    pub v: u8,
    #[serde(rename = "type")]
    pub payload_type: String,
    pub server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub credential: String,
}

impl ConnectConfigPayload {
    pub fn new(
        server_url: String,
        token_id: Option<String>,
        name: Option<String>,
        credential: String,
    ) -> Self {
        Self {
            v: CONNECT_CONFIG_VERSION,
            payload_type: CONNECT_CONFIG_TYPE.to_string(),
            server_url,
            token_id,
            name,
            credential,
        }
    }
}

pub fn normalize_connect_server_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("connect-config server URL is empty");
    }

    let url = reqwest::Url::parse(trimmed).context("connect-config server URL is invalid")?;
    if url.scheme() != "https" {
        anyhow::bail!("connect-config server URL must use https://");
    }
    if url.host_str().is_none() {
        anyhow::bail!("connect-config server URL must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("connect-config server URL must not include credentials");
    }
    if url.path() != "/" {
        anyhow::bail!("connect-config server URL must not include a path");
    }
    if url.query().is_some() {
        anyhow::bail!("connect-config server URL must not include a query string");
    }
    if url.fragment().is_some() {
        anyhow::bail!("connect-config server URL must not include a fragment");
    }

    Ok(url.origin().ascii_serialization())
}

pub fn normalize_optional_display_name(raw: Option<&str>) -> Option<String> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub fn normalize_optional_token_id(raw: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(value) = raw else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !trimmed.starts_with("cc_") {
        anyhow::bail!("connect-config token_id must start with cc_");
    }
    if trimmed.len() > MAX_CONNECT_TOKEN_ID_BYTES {
        anyhow::bail!("connect-config token_id exceeds {MAX_CONNECT_TOKEN_ID_BYTES} bytes");
    }
    Ok(Some(trimmed.to_string()))
}

pub fn build_connect_url(
    server_url: &str,
    token_id: Option<&str>,
    name: Option<&str>,
    credential: &str,
) -> anyhow::Result<String> {
    build_connect_url_with_scheme(server_url, token_id, name, credential, "cmdock")
}

/// Allowed URL schemes for connect-config URLs.
const ALLOWED_CONNECT_SCHEMES: &[&str] = &["cmdock", "cmdock-staging"];

pub fn build_connect_url_with_scheme(
    server_url: &str,
    token_id: Option<&str>,
    name: Option<&str>,
    credential: &str,
    scheme: &str,
) -> anyhow::Result<String> {
    if !ALLOWED_CONNECT_SCHEMES.contains(&scheme) {
        anyhow::bail!(
            "connect-config scheme '{scheme}' is not allowed (valid: {})",
            ALLOWED_CONNECT_SCHEMES.join(", ")
        );
    }
    if credential.trim().is_empty() {
        anyhow::bail!("connect-config credential is empty");
    }

    let payload = ConnectConfigPayload::new(
        normalize_connect_server_url(server_url)?,
        normalize_optional_token_id(token_id)?,
        normalize_optional_display_name(name),
        credential.to_string(),
    );
    let payload_json = serde_json::to_vec(&payload)?;
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json);
    let url = format!("{scheme}://connect?payload={payload_b64}");

    if url.len() > MAX_CONNECT_URL_BYTES {
        anyhow::bail!(
            "connect-config URL is {} bytes, which exceeds the {} byte budget",
            url.len(),
            MAX_CONNECT_URL_BYTES
        );
    }

    Ok(url)
}

pub fn decode_connect_url(url: &str) -> anyhow::Result<ConnectConfigPayload> {
    let parsed = reqwest::Url::parse(url).context("connect-config URL is invalid")?;
    if parsed.scheme() != "cmdock" && parsed.scheme() != "cmdock-staging" {
        anyhow::bail!("connect-config URL must use cmdock:// or cmdock-staging://");
    }
    let payload_b64 = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "payload").then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("connect-config URL is missing payload"))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("connect-config payload is not valid base64url")?;
    let mut payload: ConnectConfigPayload =
        serde_json::from_slice(&decoded).context("connect-config payload is not valid JSON")?;
    if payload.v != CONNECT_CONFIG_VERSION {
        anyhow::bail!(
            "connect-config payload version {} is not supported",
            payload.v
        );
    }
    if payload.payload_type != CONNECT_CONFIG_TYPE {
        anyhow::bail!(
            "connect-config payload type '{}' is not supported",
            payload.payload_type
        );
    }
    payload.server_url = normalize_connect_server_url(&payload.server_url)?;
    payload.token_id = normalize_optional_token_id(payload.token_id.as_deref())?;
    if payload.credential.trim().is_empty() {
        anyhow::bail!("connect-config payload credential is empty");
    }
    payload.name = normalize_optional_display_name(payload.name.as_deref());
    Ok(payload)
}

pub fn render_terminal_qr(connect_url: &str) -> anyhow::Result<String> {
    let qr = QrCode::encode_text(connect_url, QrCodeEcc::Medium)
        .map_err(|err| anyhow::anyhow!("failed to encode connect-config QR: {err:?}"))?;
    let size = qr.size();
    let border = 2;
    let mut out = String::new();

    for y in (-border..size + border).step_by(2usize) {
        for x in -border..size + border {
            let upper = qr_module(&qr, x, y);
            let lower = qr_module(&qr, x, y + 1);
            let ch = match (upper, lower) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            out.push(ch);
        }
        out.push('\n');
    }

    Ok(out)
}

fn qr_module(qr: &QrCode, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && x < qr.size() && y < qr.size() && qr.get_module(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_connect_server_url_accepts_https_origin() {
        let url = normalize_connect_server_url(" https://tasks.example.com/ ").unwrap();
        assert_eq!(url, "https://tasks.example.com");
    }

    #[test]
    fn test_normalize_connect_server_url_rejects_non_https() {
        let err = normalize_connect_server_url("http://tasks.example.com").unwrap_err();
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn test_normalize_connect_server_url_rejects_path_query_and_fragment() {
        for raw in [
            "https://tasks.example.com/api",
            "https://tasks.example.com?foo=bar",
            "https://tasks.example.com#frag",
        ] {
            assert!(normalize_connect_server_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn test_build_and_decode_connect_url_round_trip() {
        let url = build_connect_url(
            "https://tasks.example.com",
            Some("cc_0123456789abcd"),
            Some("Dogfood"),
            "abc123opaquecredential",
        )
        .unwrap();
        assert!(url.starts_with("cmdock://connect?payload="));
        let payload = decode_connect_url(&url).unwrap();
        assert_eq!(
            payload,
            ConnectConfigPayload::new(
                "https://tasks.example.com".to_string(),
                Some("cc_0123456789abcd".to_string()),
                Some("Dogfood".to_string()),
                "abc123opaquecredential".to_string(),
            )
        );
    }

    #[test]
    fn test_build_connect_url_with_staging_scheme() {
        let url = build_connect_url_with_scheme(
            "https://tasks.example.com",
            Some("cc_0123456789abcd"),
            None,
            "abc123opaquecredential",
            "cmdock-staging",
        )
        .unwrap();
        assert!(url.starts_with("cmdock-staging://connect?payload="));
        let payload = decode_connect_url(&url).unwrap();
        assert_eq!(payload.server_url, "https://tasks.example.com");
    }

    #[test]
    fn test_build_connect_url_rejects_invalid_scheme() {
        let err = build_connect_url_with_scheme(
            "https://tasks.example.com",
            Some("cc_0123456789abcd"),
            None,
            "abc123opaquecredential",
            "foo",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not allowed"),
            "expected scheme rejection, got: {err}"
        );
    }

    #[test]
    fn test_build_connect_url_enforces_byte_budget() {
        let long_name = "x".repeat(200);
        let err = build_connect_url(
            "https://tasks.example.com",
            Some("cc_0123456789abcd"),
            Some(&long_name),
            "abc123opaquecredential",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("exceeds the"),
            "expected byte budget error, got: {err}"
        );
    }

    #[test]
    fn test_render_terminal_qr_produces_unicode_blocks() {
        let qr = render_terminal_qr("cmdock://connect?payload=test").unwrap();
        assert!(!qr.trim().is_empty());
        assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
    }

    #[test]
    fn test_normalize_optional_token_id_accepts_contract_shape() {
        let token_id = normalize_optional_token_id(Some("cc_0123456789abcd")).unwrap();
        assert_eq!(token_id.as_deref(), Some("cc_0123456789abcd"));
    }

    #[test]
    fn test_normalize_optional_token_id_rejects_invalid_shape() {
        assert!(normalize_optional_token_id(Some("token-123")).is_err());
        assert!(normalize_optional_token_id(Some("cc_0123456789abcdefghi")).is_err());
    }
}
