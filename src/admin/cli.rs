//! Admin CLI subcommands for user/token/backup management.
//!
//! These commands operate directly on the config database and replica files,
//! without requiring the server to be running. Designed for self-hosters
//! who need to manage accounts without touching SQLite directly.

mod backup_restore;
mod common;
mod connect_config;
mod device;
mod sync;
mod token;
mod user;

use std::path::{Path, PathBuf};

use clap::Subcommand;

pub use backup_restore::copy_dir_recursive;

#[derive(Subcommand)]
pub enum AdminCommand {
    /// User management
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// API token management
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Replica management — per-user sync identity + key escrow (ADR-0001)
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
    /// Per-device lifecycle management
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Generate a CLI-first cmdock:// connect-config URL and terminal QR
    ConnectConfig {
        #[command(subcommand)]
        action: ConnectConfigAction,
    },
    /// Back up config database and replica files
    Backup {
        /// Output directory for backup
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore config database and replica files from backup
    Restore {
        /// Directory containing backup to restore
        #[arg(long, alias = "from")]
        input: PathBuf,
        /// Restore only a single user from the backup
        #[arg(long)]
        user_id: Option<String>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum UserAction {
    /// Create a new user account
    Create {
        /// Username for the new account
        #[arg(long)]
        username: String,
    },
    /// List all user accounts
    List,
    /// Delete a user account and all associated data
    Delete {
        /// User ID to delete
        user_id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Mark a user offline for restore/recovery work
    Offline {
        /// User ID to take offline
        user_id: String,
    },
    /// Bring a user back online after restore/recovery work
    Online {
        /// User ID to bring online
        user_id: String,
    },
    /// Assess whether a user's runtime state is healthy, rebuildable, or needs intervention
    Assess {
        /// User ID to inspect
        user_id: String,
    },
}

#[derive(Subcommand)]
pub enum TokenAction {
    /// Create a new API token for a user
    Create {
        /// User ID to create token for
        user_id: String,
        /// Optional label for the token
        #[arg(long)]
        label: Option<String>,
    },
    /// List API tokens for a user
    List {
        /// User ID to list tokens for
        user_id: String,
    },
    /// Revoke an API token by its hash prefix
    Revoke {
        /// Token hash (or unique prefix) to revoke
        token_hash: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum SyncAction {
    /// Create the canonical sync identity for a user
    Create {
        /// User ID to create the canonical sync identity for
        user_id: String,
    },
    /// Show canonical sync identity info for a user
    Show {
        /// User ID
        user_id: String,
    },
    /// Show the decrypted canonical sync secret (debug/migration use)
    #[command(hide = true)]
    ShowSecret {
        /// User ID
        user_id: String,
    },
    /// Delete a user's canonical sync identity
    Delete {
        /// User ID whose replica to remove
        user_id: String,
    },
}

#[derive(Subcommand)]
pub enum DeviceAction {
    /// List devices for a user
    List {
        /// User ID
        user_id: String,
    },
    /// Create a new device and print onboarding credentials
    Create {
        /// User ID to register the device for
        user_id: String,
        /// Human-readable device name
        #[arg(long)]
        name: String,
        /// Public sync server URL to include in the emitted snippet
        #[arg(long)]
        server_url: Option<String>,
    },
    /// Show a device record (metadata only)
    Show {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
    },
    /// Print a .taskrc-compatible snippet for an existing device
    Taskrc {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
        /// Public sync server URL to include in the emitted snippet
        #[arg(long)]
        server_url: Option<String>,
    },
    /// Rename a device
    Rename {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
        /// New device name
        #[arg(long)]
        name: String,
    },
    /// Soft-disable a device without deleting its record
    Revoke {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Re-enable a previously revoked device
    Unrevoke {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Permanently delete a device record
    Delete {
        /// User ID
        user_id: String,
        /// Device client_id or unique prefix
        client_id: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum ConnectConfigAction {
    /// Create a short-lived connect-config URL for a user
    Create {
        /// User ID
        user_id: String,
        /// Override the public HTTPS server URL included in the payload
        #[arg(long)]
        server_url: Option<String>,
        /// Optional human-readable connection name shown by clients
        #[arg(long)]
        name: Option<String>,
        /// Credential lifetime in minutes
        #[arg(long, default_value_t = 60)]
        expires_minutes: u32,
        /// Print the URL only, without rendering a terminal QR
        #[arg(long)]
        no_qr: bool,
    },
}

/// Run an admin CLI command against the config database.
pub async fn run(
    cmd: AdminCommand,
    data_dir: &Path,
    config: Option<&crate::config::ServerConfig>,
) -> anyhow::Result<()> {
    match cmd {
        AdminCommand::User { action } => user::run(action, data_dir).await,
        AdminCommand::Token { action } => token::run(action, data_dir).await,
        AdminCommand::Sync { action } => sync::run(action, data_dir).await,
        AdminCommand::Device { action } => device::run(action, data_dir).await,
        AdminCommand::ConnectConfig { action } => {
            connect_config::run(action, data_dir, config).await
        }
        AdminCommand::Backup { output } => backup_restore::run_backup(data_dir, &output).await,
        AdminCommand::Restore {
            input,
            user_id,
            yes,
        } => backup_restore::run_restore(data_dir, &input, user_id.as_deref(), yes).await,
    }
}
