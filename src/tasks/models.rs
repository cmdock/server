use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Task as returned by the API — matches iOS TaskItem model
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "description": "Buy milk",
    "project": "PERSONAL.Home",
    "tags": ["shopping", "coles"],
    "priority": "H",
    "due": "20260328T090000Z",
    "urgency": 12.47,
    "blocked": false,
    "waiting": false,
    "status": "pending"
}))]
pub struct TaskItem {
    /// Task UUID
    #[schema(format = "uuid")]
    pub uuid: String,
    pub description: String,
    /// Project name (dot-separated hierarchy, e.g. "PERSONAL.Home")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Priority level
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "H")]
    pub priority: Option<String>,
    /// Due date in Taskwarrior format (YYYYMMDDTHHmmssZ)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "20260328T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    pub due: Option<String>,
    /// Computed urgency score
    pub urgency: f64,
    /// True when the task depends on at least one pending task.
    pub blocked: bool,
    /// True when the task has a future wait date and is not yet actionable.
    pub waiting: bool,
    /// Task status
    #[schema(example = "pending")]
    pub status: String,
}

/// Response for task mutations (add, done, undo, delete, modify)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({"success": true, "output": "Created task a1b2c3d4."}))]
pub struct TaskActionResponse {
    pub success: bool,
    pub output: String,
}

/// Request body for POST /api/tasks
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({"raw": "project:PERSONAL.Home +shopping +coles Buy milk"}))]
pub struct AddTaskRequest {
    /// Raw Taskwarrior syntax: project:X +tag priority:H due:date Description
    #[garde(
        length(min = 1, max = 4096),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    )]
    pub raw: String,
}

/// Request body for POST /api/tasks/{uuid}/modify.
/// Only provided fields are updated; omitted fields are left unchanged.
#[derive(Debug, Deserialize, ToSchema, Validate)]
#[schema(example = json!({"priority": "M", "tags": ["shopping", "woolworths"], "depends": ["a1b2c3d4-e5f6-7890-abcd-ef1234567890"]}))]
pub struct ModifyTaskRequest {
    /// Due date in Taskwarrior format (YYYYMMDDTHHmmssZ), or null to clear
    #[schema(example = "20260330T090000Z", pattern = r"^\d{8}T\d{6}Z$")]
    #[garde(inner(
        length(min = 1, max = 64),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub due: Option<String>,
    /// Priority: H (high), M (medium), L (low), or null to clear
    #[schema(example = "M")]
    #[garde(inner(
        length(min = 1, max = 8),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub priority: Option<String>,
    /// Project name (dot-separated hierarchy)
    #[schema(example = "PERSONAL.Health")]
    #[garde(inner(
        length(min = 1, max = 255),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub project: Option<String>,
    /// Replace all tags with this list
    #[garde(custom(crate::validation::optional_tag_list))]
    pub tags: Option<Vec<String>>,
    /// Replace all task dependencies with this list of task UUIDs.
    /// Use an empty array to clear all dependencies.
    #[garde(custom(crate::validation::optional_uuid_list))]
    pub depends: Option<Vec<String>>,
    /// Task description
    #[garde(inner(
        length(min = 1, max = 4096),
        custom(crate::validation::trimmed_non_empty),
        custom(crate::validation::no_control_chars)
    ))]
    pub description: Option<String>,
}
