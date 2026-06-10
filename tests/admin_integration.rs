//! Feature-split integration tests for admin endpoints.

#[path = "admin_integration/backup_restore.rs"]
mod backup_restore;
mod common;
#[path = "admin_integration/connect_and_devices.rs"]
mod connect_and_devices;
#[path = "admin_integration/runtime_policy.rs"]
mod runtime_policy;
#[path = "admin_integration/status_and_ops.rs"]
mod status_and_ops;
#[path = "admin_integration/support.rs"]
mod support;
