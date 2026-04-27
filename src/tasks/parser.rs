//! Parse Taskwarrior raw syntax into structured task fields.
//!
//! Handles input like: `project:PERSONAL.Home +shopping +coles Buy milk`

/// Parsed fields from a raw Taskwarrior command string
#[derive(Debug, Default)]
pub struct ParsedTask {
    pub description: String,
    pub project: Option<String>,
    pub tags: Vec<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
}

/// Parse a raw Taskwarrior add command string into structured fields.
///
/// Recognises:
/// - `project:VALUE` — project assignment
/// - `+TAG` — tag addition
/// - `priority:H/M/L` — priority
/// - `due:VALUE` — due date
/// - Everything else — description
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
}
