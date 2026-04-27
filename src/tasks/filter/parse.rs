//! Filter expression parser.
//!
//! Parses Taskwarrior filter strings into a FilterExpr AST.
//! Handles implicit AND between adjacent terms, parenthesised groups,
//! and `or`/`and` operators.

use super::dates::parse_date_value;
use super::tokens::{AttrModifier, FilterExpr, DATE_ATTRIBUTES};

/// Parse a Taskwarrior filter expression string into a FilterExpr AST.
pub fn parse_filter(input: &str) -> FilterExpr {
    let tokens = tokenize(input);
    if tokens.is_empty() {
        return FilterExpr::True;
    }
    let mut pos = 0;
    parse_or(&tokens, &mut pos)
}

/// Raw token from the filter string (before AST construction).
#[derive(Debug, Clone)]
enum RawToken {
    /// attribute.modifier:value or attribute:value
    Attr {
        name: String,
        modifier: AttrModifier,
        value: String,
        parsed_date: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// +tag
    HasTag(String),
    /// -tag (but not a negative number)
    NotTag(String),
    /// Boolean keyword
    And,
    Or,
    Not,
    /// Parentheses
    LParen,
    RParen,
}

/// Tokenize a filter string into raw tokens.
fn tokenize(input: &str) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut buf = String::new();

    while let Some(&ch) = chars.peek() {
        match ch {
            ' ' | '\t' => {
                flush_word(&buf, &mut tokens);
                buf.clear();
                chars.next();
            }
            '(' => {
                flush_word(&buf, &mut tokens);
                buf.clear();
                tokens.push(RawToken::LParen);
                chars.next();
            }
            ')' => {
                flush_word(&buf, &mut tokens);
                buf.clear();
                tokens.push(RawToken::RParen);
                chars.next();
            }
            _ => {
                buf.push(ch);
                chars.next();
            }
        }
    }
    flush_word(&buf, &mut tokens);

    // Insert implicit AND between adjacent non-operator tokens
    insert_implicit_and(&mut tokens);

    tokens
}

/// Flush accumulated word buffer into a token.
fn flush_word(word: &str, tokens: &mut Vec<RawToken>) {
    if word.is_empty() {
        return;
    }

    // Case-insensitive keyword matching without allocation
    if word.eq_ignore_ascii_case("and") {
        tokens.push(RawToken::And);
        return;
    }
    if word.eq_ignore_ascii_case("or") {
        tokens.push(RawToken::Or);
        return;
    }
    if word.eq_ignore_ascii_case("not") || word == "!" {
        tokens.push(RawToken::Not);
        return;
    }

    // +tag
    if let Some(tag) = word.strip_prefix('+') {
        tokens.push(RawToken::HasTag(tag.to_string()));
        return;
    }

    // -tag (but not if it looks like a negative number)
    if let Some(tag) = word.strip_prefix('-') {
        if !tag.is_empty() && !tag.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            tokens.push(RawToken::NotTag(tag.to_string()));
            return;
        }
    }

    // attribute.modifier:value or attribute:value
    if let Some(colon_pos) = word.find(':') {
        let attr_part = &word[..colon_pos];
        let value = &word[colon_pos + 1..];

        // Normalise attribute name to lowercase once (case-insensitive attributes)
        let (name, modifier) = if let Some(dot_pos) = attr_part.find('.') {
            let name = attr_part[..dot_pos].to_ascii_lowercase();
            let mod_str = &attr_part[dot_pos + 1..];
            (name, parse_modifier(mod_str))
        } else {
            (attr_part.to_ascii_lowercase(), AttrModifier::Equals)
        };

        // Pre-parse date values for known date attributes (avoids per-task parsing)
        let parsed_date = if DATE_ATTRIBUTES.contains(&name.as_str()) {
            parse_date_value(value)
        } else {
            None
        };

        tokens.push(RawToken::Attr {
            name,
            modifier,
            value: value.to_string(),
            parsed_date,
        });
        return;
    }

    // Bare word — treat as description search
    tokens.push(RawToken::Attr {
        name: "description".to_string(),
        modifier: AttrModifier::Has,
        value: word.to_string(),
        parsed_date: None,
    });
}

fn parse_modifier(s: &str) -> AttrModifier {
    // Case-insensitive matching without allocation
    if s.eq_ignore_ascii_case("is") || s.eq_ignore_ascii_case("equals") {
        AttrModifier::Is
    } else if s.eq_ignore_ascii_case("isnt") || s.eq_ignore_ascii_case("not") {
        AttrModifier::Isnt
    } else if s.eq_ignore_ascii_case("before")
        || s.eq_ignore_ascii_case("under")
        || s.eq_ignore_ascii_case("below")
    {
        AttrModifier::Before
    } else if s.eq_ignore_ascii_case("after")
        || s.eq_ignore_ascii_case("over")
        || s.eq_ignore_ascii_case("above")
    {
        AttrModifier::After
    } else if s.eq_ignore_ascii_case("by") {
        AttrModifier::By
    } else if s.eq_ignore_ascii_case("has") || s.eq_ignore_ascii_case("contains") {
        AttrModifier::Has
    } else if s.eq_ignore_ascii_case("hasnt") {
        AttrModifier::Hasnt
    } else if s.eq_ignore_ascii_case("startswith") || s.eq_ignore_ascii_case("left") {
        AttrModifier::StartsWith
    } else if s.eq_ignore_ascii_case("endswith") || s.eq_ignore_ascii_case("right") {
        AttrModifier::EndsWith
    } else if s.eq_ignore_ascii_case("none") {
        AttrModifier::None
    } else if s.eq_ignore_ascii_case("any") {
        AttrModifier::Any
    } else {
        AttrModifier::Equals
    }
}

/// Insert implicit AND between adjacent primary tokens.
/// Builds a new vec in O(n) instead of repeated Vec::insert which is O(n²).
fn insert_implicit_and(tokens: &mut Vec<RawToken>) {
    let mut result = Vec::with_capacity(tokens.len() * 2);
    for (i, token) in tokens.iter().enumerate() {
        if i > 0
            && matches!(
                &tokens[i - 1],
                RawToken::Attr { .. }
                    | RawToken::HasTag(_)
                    | RawToken::NotTag(_)
                    | RawToken::RParen
            )
            && matches!(
                token,
                RawToken::Attr { .. }
                    | RawToken::HasTag(_)
                    | RawToken::NotTag(_)
                    | RawToken::LParen
                    | RawToken::Not
            )
        {
            result.push(RawToken::And);
        }
        result.push(token.clone());
    }
    *tokens = result;
}

// --- Recursive descent parser ---

/// Parse OR expression: and_expr { "or" and_expr }
fn parse_or(tokens: &[RawToken], pos: &mut usize) -> FilterExpr {
    let mut left = parse_and(tokens, pos);
    while *pos < tokens.len() {
        if matches!(tokens[*pos], RawToken::Or) {
            *pos += 1;
            let right = parse_and(tokens, pos);
            left = FilterExpr::Or(Box::new(left), Box::new(right));
        } else {
            break;
        }
    }
    left
}

/// Parse AND expression: unary { "and" unary }
fn parse_and(tokens: &[RawToken], pos: &mut usize) -> FilterExpr {
    let mut left = parse_unary(tokens, pos);
    while *pos < tokens.len() {
        if matches!(tokens[*pos], RawToken::And) {
            *pos += 1;
            let right = parse_unary(tokens, pos);
            left = FilterExpr::And(Box::new(left), Box::new(right));
        } else {
            break;
        }
    }
    left
}

/// Parse unary: "not" unary | primary
fn parse_unary(tokens: &[RawToken], pos: &mut usize) -> FilterExpr {
    if *pos < tokens.len() && matches!(tokens[*pos], RawToken::Not) {
        *pos += 1;
        let expr = parse_unary(tokens, pos);
        return FilterExpr::Not(Box::new(expr));
    }
    parse_primary(tokens, pos)
}

/// Parse primary: "(" or_expr ")" | atom
fn parse_primary(tokens: &[RawToken], pos: &mut usize) -> FilterExpr {
    if *pos >= tokens.len() {
        return FilterExpr::True;
    }

    match &tokens[*pos] {
        RawToken::LParen => {
            *pos += 1;
            let expr = parse_or(tokens, pos);
            if *pos < tokens.len() && matches!(tokens[*pos], RawToken::RParen) {
                *pos += 1;
            }
            expr
        }
        RawToken::Attr {
            name,
            modifier,
            value,
            parsed_date,
        } => {
            let expr = FilterExpr::Attribute {
                name: name.clone(),
                modifier: modifier.clone(),
                value: value.clone(),
                parsed_date: *parsed_date,
            };
            *pos += 1;
            expr
        }
        RawToken::HasTag(tag) => {
            let expr = FilterExpr::HasTag(tag.clone());
            *pos += 1;
            expr
        }
        RawToken::NotTag(tag) => {
            let expr = FilterExpr::NotTag(tag.clone());
            *pos += 1;
            expr
        }
        _ => {
            *pos += 1;
            FilterExpr::True
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_status() {
        let expr = parse_filter("status:pending");
        match expr {
            FilterExpr::Attribute { name, value, .. } => {
                assert_eq!(name, "status");
                assert_eq!(value, "pending");
            }
            _ => panic!("Expected Attribute"),
        }
    }

    #[test]
    fn test_implicit_and() {
        let expr = parse_filter("status:pending +shopping");
        assert!(matches!(expr, FilterExpr::And(_, _)));
    }

    #[test]
    fn test_or_expression() {
        let expr = parse_filter("+DUETODAY or +OVERDUE");
        assert!(matches!(expr, FilterExpr::Or(_, _)));
    }

    #[test]
    fn test_parenthesised() {
        let expr = parse_filter("status:pending (+DUETODAY or +OVERDUE)");
        match expr {
            FilterExpr::And(left, right) => {
                assert!(matches!(*left, FilterExpr::Attribute { .. }));
                assert!(matches!(*right, FilterExpr::Or(_, _)));
            }
            _ => panic!("Expected And(Attribute, Or(...))"),
        }
    }

    #[test]
    fn test_modifier() {
        let expr = parse_filter("project.startswith:PERSONAL");
        match expr {
            FilterExpr::Attribute {
                name,
                modifier,
                value,
                ..
            } => {
                assert_eq!(name, "project");
                assert_eq!(modifier, AttrModifier::StartsWith);
                assert_eq!(value, "PERSONAL");
            }
            _ => panic!("Expected Attribute with StartsWith"),
        }
    }

    #[test]
    fn test_empty() {
        let expr = parse_filter("");
        assert!(matches!(expr, FilterExpr::True));
    }

    #[test]
    fn test_complex_ios_filter() {
        // Real filter from the spec
        let expr = parse_filter("status:pending +DUETODAY or +OVERDUE or +DUE");
        // This should parse as: (status:pending AND +DUETODAY) OR +OVERDUE OR +DUE
        // because implicit AND binds tighter than explicit OR
        assert!(matches!(expr, FilterExpr::Or(_, _)));
    }
}
