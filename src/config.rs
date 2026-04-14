use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub server: ServerSection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default = "default_backup_dir")]
    pub backup_dir: Option<PathBuf>,
    #[serde(default = "default_backup_retention_count")]
    pub backup_retention_count: usize,
    #[serde(default)]
    pub llm: Option<LlmSection>,
    #[serde(default)]
    pub audit: AuditSection,
    /// Master encryption key for envelope encryption of sync secrets.
    /// Set via CMDOCK_MASTER_KEY env var (hex or base64, 32 bytes).
    /// Required when replicas exist. None = no encryption support.
    #[serde(skip)]
    pub master_key: Option<[u8; 32]>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AdminSection {
    /// Bearer token for operator HTTP endpoints under `/admin/*`.
    /// Set via CMDOCK_ADMIN_TOKEN for self-hosted/staging use.
    #[serde(default)]
    pub http_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuditSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audit_output")]
    pub output: String,
}

impl Default for AuditSection {
    fn default() -> Self {
        Self {
            enabled: false,
            output: default_audit_output(),
        }
    }
}

fn default_audit_output() -> String {
    "stderr".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerSection {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub public_base_url: Option<String>,
    #[serde(default)]
    pub trust_forwarded_headers: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmSection {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_cache_ttl")]
    pub summary_cache_ttl_secs: u64,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}
fn default_provider() -> String {
    "anthropic".to_string()
}
fn default_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}
fn default_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}
fn default_cache_ttl() -> u64 {
    300
}
fn default_backup_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/data/cmdock/backups"))
}
fn default_backup_retention_count() -> usize {
    7
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;
        config.apply_env_overrides();
        Ok(config)
    }

    pub fn public_server_url(&self, override_url: Option<&str>) -> anyhow::Result<String> {
        let raw = override_url
            .or(self.server.public_base_url.as_deref())
            .ok_or_else(|| anyhow::anyhow!("public server URL is not configured"))?;
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            anyhow::bail!("public server URL is empty");
        }
        if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
            anyhow::bail!("public server URL must start with http:// or https://");
        }
        Ok(trimmed.to_string())
    }

    /// Apply environment variable overrides (12-factor app pattern).
    /// Env vars take precedence over config file values.
    ///
    /// Supported:
    ///   CMDOCK_HOST         → server.host
    ///   CMDOCK_PORT         → server.port
    ///   CMDOCK_DATA_DIR     → server.data_dir
    ///   CMDOCK_PUBLIC_BASE_URL → server.public_base_url
    ///   CMDOCK_TRUST_FORWARDED_HEADERS → server.trust_forwarded_headers
    ///   CMDOCK_BACKUP_DIR   → backup_dir
    ///   CMDOCK_BACKUP_RETENTION_COUNT → backup_retention_count
    ///   CMDOCK_MASTER_KEY    → master_key (hex or base64, 32 bytes)
    ///   CMDOCK_AUDIT_ENABLED → audit.enabled ("true"/"1" = on)
    ///   CMDOCK_AUDIT_OUTPUT  → audit.output
    ///   CMDOCK_ADMIN_TOKEN   → admin.http_token
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("CMDOCK_HOST") {
            self.server.host = v;
        }
        if let Ok(v) = std::env::var("CMDOCK_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                self.server.port = port;
            }
        }
        if let Ok(v) = std::env::var("CMDOCK_DATA_DIR") {
            self.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("CMDOCK_PUBLIC_BASE_URL") {
            self.server.public_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("CMDOCK_TRUST_FORWARDED_HEADERS") {
            self.server.trust_forwarded_headers = matches!(v.as_str(), "true" | "1" | "yes");
        }
        if let Ok(v) = std::env::var("CMDOCK_BACKUP_DIR") {
            self.backup_dir = if v.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            };
        }
        if let Ok(v) = std::env::var("CMDOCK_BACKUP_RETENTION_COUNT") {
            if let Ok(retention) = v.parse::<usize>() {
                self.backup_retention_count = retention;
            }
        }
        if let Ok(v) = std::env::var("CMDOCK_AUDIT_ENABLED") {
            self.audit.enabled = matches!(v.as_str(), "true" | "1" | "yes");
        }
        if let Ok(v) = std::env::var("CMDOCK_AUDIT_OUTPUT") {
            self.audit.output = v;
        }
        if let Ok(v) = std::env::var("CMDOCK_ADMIN_TOKEN") {
            self.admin.http_token = Some(v);
        }
        if let Ok(v) = std::env::var("CMDOCK_MASTER_KEY") {
            match crate::crypto::parse_master_key(&v) {
                Ok(key) => self.master_key = Some(key),
                Err(e) => tracing::warn!("Invalid CMDOCK_MASTER_KEY: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a default ServerConfig for testing (no file needed).
    fn default_config() -> ServerConfig {
        ServerConfig {
            server: ServerSection {
                host: default_host(),
                port: default_port(),
                data_dir: default_data_dir(),
                public_base_url: None,
                trust_forwarded_headers: false,
            },
            admin: AdminSection::default(),
            backup_dir: default_backup_dir(),
            backup_retention_count: default_backup_retention_count(),
            llm: None,
            audit: AuditSection::default(),
            master_key: None,
        }
    }

    /// Serialises the five config-load tests that mutate or read the
    /// process-global env vars (`CMDOCK_HOST`, `CMDOCK_PORT`, ...).
    /// `std::env::set_var` / `remove_var` are not thread-safe, and cargo
    /// runs tests in parallel by default — without this lock, concurrent
    /// test_env_overrides and test_load_* would race on the same variables
    /// and produce intermittent failures in `just check` / `cargo test`.
    ///
    /// Cheaper than adding `serial_test` as a dev-dep for one module.
    /// Poisoned on panic is recovered with `into_inner` so a single bad
    /// test doesn't cascade into the rest.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_test_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn test_load_minimal_config() {
        let _env_guard = env_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("minimal.toml");
        std::fs::write(
            &path,
            "[server]\nhost = \"127.0.0.1\"\nport = 3000\ndata_dir = \"./mydata\"\n",
        )
        .unwrap();
        // Clear env vars that apply_env_overrides() might pick up from parallel tests
        for var in &[
            "CMDOCK_HOST",
            "CMDOCK_PORT",
            "CMDOCK_DATA_DIR",
            "CMDOCK_AUDIT_ENABLED",
            "CMDOCK_AUDIT_OUTPUT",
        ] {
            unsafe { std::env::remove_var(var) };
        }
        let cfg = ServerConfig::load(&path).unwrap();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 3000);
        assert_eq!(cfg.server.data_dir, PathBuf::from("./mydata"));
        assert!(cfg.llm.is_none(), "llm should default to None");
        assert!(!cfg.audit.enabled, "audit should default to disabled");
        assert_eq!(
            cfg.audit.output, "stderr",
            "audit output should default to stderr even when [audit] section omitted"
        );
    }

    #[test]
    fn test_load_with_all_sections() {
        let _env_guard = env_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.toml");
        std::fs::write(
            &path,
            r#"
backup_dir = "/data/cmdock/backups"
backup_retention_count = 9

[server]
host = "0.0.0.0"
port = 9090
data_dir = "/data"
public_base_url = "https://tasks.example.com"
trust_forwarded_headers = true

[admin]
http_token = "operator-secret"

[llm]
provider = "openai"
model = "gpt-4"
api_key_env = "OPENAI_KEY"
summary_cache_ttl_secs = 600

[audit]
enabled = true
output = "/var/log/audit.jsonl"
"#,
        )
        .unwrap();
        // Clear env vars that apply_env_overrides() might pick up from parallel tests
        for var in &[
            "CMDOCK_HOST",
            "CMDOCK_PORT",
            "CMDOCK_DATA_DIR",
            "CMDOCK_AUDIT_ENABLED",
            "CMDOCK_AUDIT_OUTPUT",
        ] {
            unsafe { std::env::remove_var(var) };
        }
        let cfg = ServerConfig::load(&path).unwrap();
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 9090);
        assert_eq!(cfg.server.data_dir, PathBuf::from("/data"));
        assert_eq!(
            cfg.server.public_base_url.as_deref(),
            Some("https://tasks.example.com")
        );
        assert!(cfg.server.trust_forwarded_headers);
        assert_eq!(cfg.admin.http_token.as_deref(), Some("operator-secret"));
        assert_eq!(cfg.backup_dir, Some(PathBuf::from("/data/cmdock/backups")));
        assert_eq!(cfg.backup_retention_count, 9);
        let llm = cfg.llm.as_ref().unwrap();
        assert_eq!(llm.provider, "openai");
        assert_eq!(llm.model, "gpt-4");
        assert_eq!(llm.api_key_env, "OPENAI_KEY");
        assert_eq!(llm.summary_cache_ttl_secs, 600);
        assert!(cfg.audit.enabled);
        assert_eq!(cfg.audit.output, "/var/log/audit.jsonl");
    }

    #[test]
    fn test_load_missing_server_section() {
        let _env_guard = env_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_server.toml");
        std::fs::write(&path, "[audit]\nenabled = true\n").unwrap();
        let result = ServerConfig::load(&path);
        assert!(
            result.is_err(),
            "config without [server] section should fail to parse"
        );
    }

    #[test]
    fn test_load_invalid_port_type() {
        let _env_guard = env_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad_port.toml");
        std::fs::write(
            &path,
            "[server]\nhost = \"0.0.0.0\"\nport = \"not_a_number\"\ndata_dir = \"./data\"\n",
        )
        .unwrap();
        let result = ServerConfig::load(&path);
        assert!(result.is_err(), "non-numeric port should fail TOML parsing");
    }

    /// All env override tests run in a single function to avoid parallel
    /// test races — `std::env::set_var` / `remove_var` mutate process-wide
    /// state and are not safe to call concurrently.
    #[test]
    fn test_env_overrides() {
        let _env_guard = env_test_guard();
        // --- host ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_HOST", "127.0.0.1") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_HOST") };
        assert_eq!(cfg.server.host, "127.0.0.1");

        // --- port (valid) ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_PORT", "9999") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_PORT") };
        assert_eq!(cfg.server.port, 9999);

        // --- port (invalid — should leave value unchanged) ---
        let mut cfg = default_config();
        let original_port = cfg.server.port;
        unsafe { std::env::set_var("CMDOCK_PORT", "notanumber") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_PORT") };
        assert_eq!(
            cfg.server.port, original_port,
            "invalid port should leave value unchanged"
        );

        // --- data_dir ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_DATA_DIR", "/tmp/custom-data") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_DATA_DIR") };
        assert_eq!(cfg.server.data_dir, PathBuf::from("/tmp/custom-data"));

        // --- public_base_url ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_PUBLIC_BASE_URL", "https://tasks.example.com") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_PUBLIC_BASE_URL") };
        assert_eq!(
            cfg.server.public_base_url.as_deref(),
            Some("https://tasks.example.com")
        );

        // --- trust_forwarded_headers ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_TRUST_FORWARDED_HEADERS", "yes") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_TRUST_FORWARDED_HEADERS") };
        assert!(cfg.server.trust_forwarded_headers);

        // --- backup_dir ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_BACKUP_DIR", "/tmp/cmdock-backups") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_BACKUP_DIR") };
        assert_eq!(cfg.backup_dir, Some(PathBuf::from("/tmp/cmdock-backups")));

        // --- backup retention count ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_BACKUP_RETENTION_COUNT", "12") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_BACKUP_RETENTION_COUNT") };
        assert_eq!(cfg.backup_retention_count, 12);

        // --- audit enabled (true) ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_AUDIT_ENABLED", "true") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_AUDIT_ENABLED") };
        assert!(cfg.audit.enabled, "audit should be enabled for 'true'");

        // --- audit enabled (false for 'no') ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_AUDIT_ENABLED", "no") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_AUDIT_ENABLED") };
        assert!(!cfg.audit.enabled, "audit should not be enabled for 'no'");

        // --- audit enabled (truthy variants: "1", "yes") ---
        for val in &["1", "yes"] {
            let mut cfg = default_config();
            unsafe { std::env::set_var("CMDOCK_AUDIT_ENABLED", val) };
            cfg.apply_env_overrides();
            unsafe { std::env::remove_var("CMDOCK_AUDIT_ENABLED") };
            assert!(cfg.audit.enabled, "audit should be enabled for '{val}'");
        }

        // --- audit output ---
        let mut cfg = default_config();
        unsafe { std::env::set_var("CMDOCK_AUDIT_OUTPUT", "/var/log/audit.jsonl") };
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var("CMDOCK_AUDIT_OUTPUT") };
        assert_eq!(cfg.audit.output, "/var/log/audit.jsonl");
    }
}
