use serde::{Deserialize, Serialize};

use crate::runtime_policy::RuntimePolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub created_at: String,
}

pub struct NewUser {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicyRecord {
    pub user_id: String,
    pub desired_version: String,
    pub desired_policy: RuntimePolicy,
    pub applied_version: Option<String>,
    pub applied_policy: Option<RuntimePolicy>,
    pub applied_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewRecord {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub filter: String,
    pub group_by: Option<String>,
    pub context_filtered: bool,
    /// Display mode: "list" (flat), "grouped" (grouped by project/tags)
    #[serde(default = "default_display_mode")]
    pub display_mode: String,
    pub sort_order: i32,
    /// "builtin" (seeded by server) or "user" (created via API)
    #[serde(default = "default_origin")]
    pub origin: String,
    /// True if user has customised a builtin view's filter/label/icon
    #[serde(default)]
    pub user_modified: bool,
    /// True if user explicitly deleted a builtin view (tombstone — prevents re-seeding)
    #[serde(default)]
    pub hidden: bool,
    /// Which default viewset version created/last updated this builtin view
    #[serde(default)]
    pub template_version: i32,
    /// Binds a context-filtered view to a specific ContextDefinition.
    /// When set, clients auto-apply the bound context's projectPrefixes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
}

fn default_display_mode() -> String {
    "list".to_string()
}

fn default_origin() -> String {
    "user".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRecord {
    pub id: String,
    pub label: String,
    /// JSON-encoded array of project prefixes
    pub project_prefixes: Vec<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetRecord {
    pub id: String,
    pub label: String,
    pub raw_suffix: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreRecord {
    pub id: String,
    pub label: String,
    pub tag: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShoppingRecord {
    pub project: String,
    pub default_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeofenceRecord {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius: f64,
    #[serde(rename = "type")]
    pub geofence_type: String,
    pub context_id: Option<String>,
    pub view_id: Option<String>,
    pub store_tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericConfigRecord {
    pub version: Option<String>,
    pub items_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaRecord {
    /// UUID, also used as client_id for TC sync protocol
    pub id: String,
    pub user_id: String,
    /// Encryption secret encrypted with master key (base64-encoded ciphertext)
    pub encryption_secret_enc: String,
    pub label: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTokenRecord {
    pub token_hash: String,
    pub user_id: String,
    pub label: Option<String>,
    pub token_id: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub first_used_at: Option<String>,
    pub last_used_at: Option<String>,
    pub last_used_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectConfigTokenCorrelation {
    pub user_id: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub expires_at: Option<String>,
    pub is_expired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectConfigIssuedToken {
    pub token: String,
    pub token_id: String,
    pub credential_hash_prefix: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectConfigTokenUse {
    NotConnectConfig,
    FirstUse(ConnectConfigTokenCorrelation),
    RepeatUse(ConnectConfigTokenCorrelation),
}

/// A registered device (physical client that syncs via TC protocol).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// TC client_id (UUID) — identifies this device in sync requests
    pub client_id: String,
    pub user_id: String,
    pub name: String,
    /// Per-device encryption secret, encrypted with master key (base64).
    pub encryption_secret_enc: Option<String>,
    pub registered_at: String,
    pub last_sync_at: Option<String>,
    pub last_sync_ip: Option<String>,
    /// "active" or "revoked"
    pub status: String,
    pub bootstrap_request_id: Option<String>,
    pub bootstrap_status: Option<String>,
    pub bootstrap_requested_username: Option<String>,
    pub bootstrap_create_user_if_missing: Option<bool>,
    pub bootstrap_expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: u32,
    pub secret_enc: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminWebhookRecord {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub consecutive_failures: u32,
    pub secret_enc: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct NewWebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: String,
}

#[derive(Debug, Clone)]
pub struct NewAdminWebhookRecord {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: String,
}

#[derive(Debug, Clone)]
pub struct UpdateWebhookRecord {
    pub id: String,
    pub user_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub modified_fields: Option<Vec<String>>,
    pub name: Option<String>,
    pub enabled: bool,
    pub secret_enc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateAdminWebhookRecord {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDeliveryRecord {
    pub delivery_id: String,
    pub webhook_id: String,
    pub event_id: String,
    pub event: String,
    pub timestamp: String,
    pub status: String,
    pub response_status: Option<u16>,
    pub attempt: u32,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookFailureState {
    pub consecutive_failures: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookEventHistoryRecord {
    pub user_id: String,
    pub task_uuid: String,
    pub event_type: String,
    pub due_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookSyncSummary {
    pub tasks_changed: usize,
    pub created: usize,
    pub completed: usize,
    pub deleted: usize,
    pub modified: usize,
}
