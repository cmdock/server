//! Parse Taskwarrior raw syntax into structured task fields.
//!
//! Handles input like: `project:PERSONAL.Home +shopping +coles Buy milk`
//!
//! Recognised attribute set is governed by `task-write-contract.md`
//! § Recognised raw-syntax attributes. Unknown `name:value` tokens fall
//! through to the description (lenient-drop deviation, also documented in
//! the contract).

/// Parsed fields from a raw Taskwarrior command string
#[derive(Debug, Default)]
pub struct ParsedTask {
    pub description: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub wait: Option<String>,
    pub scheduled: Option<String>,
    pub cmdock_task_scope: Option<String>,
    pub cmdock_account: Option<String>,
}

/// Parse a raw Taskwarrior add command string into structured fields.
///
/// Recognises (closed set in v1, per `task-write-contract.md`):
/// - `project:VALUE` — project assignment
/// - `+TAG` — tag addition
/// - `priority:H/M/L` — priority
/// - `due:VALUE` — due date (broader date parser applied downstream)
/// - `wait:VALUE` — wait-until date (broader date parser applied downstream)
/// - `scheduled:VALUE` — scheduled-for date (broader date parser applied downstream)
/// - `cmdock_task_scope:PREFIX` — canonical Task Scope prefix assertion
/// - `cmdock_account:PREFIX` — deprecated compatibility prefix assertion
/// - Everything else — description (lenient-drop on unrecognised `name:value`)
pub fn parse_raw(raw: &str) -> ParsedTask {
    let mut parsed = ParsedTask::default();
    let mut description_parts = Vec::new();

    for token in raw.split_whitespace() {
        if let Some(project) = token.strip_prefix("project:") {
            parsed.project = Some(project.to_string());
        } else if let Some(tag) = token.strip_prefix('+') {
            parsed.tags.push(tag.to_string());
        } else if let Some(priority) = token.strip_prefix("priority:") {
            parsed.priority = Some(priority.to_uppercase());
        } else if let Some(due) = token.strip_prefix("due:") {
            parsed.due = Some(due.to_string());
        } else if let Some(wait) = token.strip_prefix("wait:") {
            parsed.wait = Some(wait.to_string());
        } else if let Some(scheduled) = token.strip_prefix("scheduled:") {
            parsed.scheduled = Some(scheduled.to_string());
        } else if let Some(scope) = token.strip_prefix("cmdock_task_scope:") {
            parsed.cmdock_task_scope = Some(scope.to_string());
        } else if let Some(account) = token.strip_prefix("cmdock_account:") {
            parsed.cmdock_account = Some(account.to_string());
        } else {
            description_parts.push(token);
        }
    }

    parsed.description = description_parts.join(" ");
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let parsed = parse_raw("Buy milk");
        assert_eq!(parsed.description, "Buy milk");
        assert!(parsed.project.is_none());
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn test_parse_full() {
        let parsed =
            parse_raw("project:PERSONAL.Home +shopping +coles priority:H due:friday Buy milk");
        assert_eq!(parsed.description, "Buy milk");
        assert_eq!(parsed.project.unwrap(), "PERSONAL.Home");
        assert_eq!(parsed.tags, vec!["shopping", "coles"]);
        assert_eq!(parsed.priority.unwrap(), "H");
        assert_eq!(parsed.due.unwrap(), "friday");
    }

    #[test]
    fn test_parse_tags_only() {
        let parsed = parse_raw("+urgent +work Review PR");
        assert_eq!(parsed.description, "Review PR");
        assert_eq!(parsed.tags, vec!["urgent", "work"]);
    }

    #[test]
    fn test_unknown_key_value_stays_in_description() {
        // Unknown key:value tokens (including UDA-like ones) stay in description.
        // UDAs are set via direct TC writes, not the raw parser.
        let parsed = parse_raw("estimate:large Buy milk");
        assert_eq!(parsed.description, "estimate:large Buy milk");
    }

    #[test]
    fn test_urls_stay_in_description() {
        let parsed = parse_raw("Review https://example.com at 12:30");
        assert_eq!(parsed.description, "Review https://example.com at 12:30");
    }

    #[test]
    fn test_parse_wait() {
        let parsed = parse_raw("wait:7d Defer this");
        assert_eq!(parsed.description, "Defer this");
        assert_eq!(parsed.wait.as_deref(), Some("7d"));
        assert!(parsed.scheduled.is_none());
    }

    #[test]
    fn test_parse_scheduled() {
        let parsed = parse_raw("scheduled:tomorrow Plan ahead");
        assert_eq!(parsed.description, "Plan ahead");
        assert_eq!(parsed.scheduled.as_deref(), Some("tomorrow"));
        assert!(parsed.wait.is_none());
    }

    #[test]
    fn test_parse_all_date_attributes_together() {
        let parsed = parse_raw("project:WORK due:friday wait:7d scheduled:monday Review proposal");
        assert_eq!(parsed.description, "Review proposal");
        assert_eq!(parsed.project.as_deref(), Some("WORK"));
        assert_eq!(parsed.due.as_deref(), Some("friday"));
        assert_eq!(parsed.wait.as_deref(), Some("7d"));
        assert_eq!(parsed.scheduled.as_deref(), Some("monday"));
    }

    #[test]
    fn test_unknown_date_attrs_stay_in_description() {
        // recur, until, depends are NOT in the recognised set per the contract;
        // they fall through to the description (lenient-drop deviation).
        let parsed = parse_raw("recur:weekly until:eom Plan retro");
        assert_eq!(parsed.description, "recur:weekly until:eom Plan retro");
        assert!(parsed.due.is_none());
        assert!(parsed.wait.is_none());
        assert!(parsed.scheduled.is_none());
    }
}
