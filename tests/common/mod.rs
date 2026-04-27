#![allow(dead_code)]

use std::path::PathBuf;

use cmdock_server::config::{AdminSection, AuditSection, ServerConfig, ServerSection};

pub fn test_server_config(data_dir: PathBuf) -> ServerConfig {
    ServerConfig {
        server: ServerSection {
            host: "127.0.0.1".to_string(),
            port: 0,
            data_dir: data_dir.clone(),
            public_base_url: Some("https://test.invalid".to_string()),
            trust_forwarded_headers: false,
        },
        admin: AdminSection::default(),
        backup_dir: Some(data_dir.join("backups")),
        backup_retention_count: 7,
        llm: None,
        audit: AuditSection::default(),
        master_key: None,
    }
}

pub fn test_server_config_with_admin_token(
    data_dir: PathBuf,
    http_token: impl Into<String>,
) -> ServerConfig {
    let mut config = test_server_config(data_dir);
    config.admin = AdminSection {
        http_token: Some(http_token.into()),
    };
    config
}

pub fn test_server_config_with_master_key(data_dir: PathBuf, master_key: [u8; 32]) -> ServerConfig {
    let mut config = test_server_config(data_dir);
    config.master_key = Some(master_key);
    config
}
