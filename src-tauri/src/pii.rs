//! PII redaction applied to result rows before they leave the MCP server.
//!
//! Redaction runs in two stages, per cell:
//!
//! 1. **Column name heuristic** — if the column name contains a known
//!    PII-ish substring (case-insensitive: "email", "phone", "ssn",
//!    "password", "first_name", "last_name", ...), the whole cell is
//!    replaced with `[REDACTED]`. This is the primary signal.
//! 2. **Value pattern heuristic** — if the cell value itself looks like
//!    an email, phone number, SSN, or credit-card-shaped digit run, it
//!    is replaced with `[REDACTED]`. This catches PII buried in
//!    generically-named columns like `notes` or `description`.
//!
//! Redaction is whole-cell (never partial) so a model on the other end
//! of the MCP pipe cannot fingerprint the unredacted portion.

const PII_COLUMN_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "email",
    "e_mail",
    "phone",
    "mobile",
    "tel_no",
    "telephone",
    "ssn",
    "social_security",
    "credit_card",
    "card_number",
    "cc_num",
    "cardnum",
    "cvv",
    "first_name",
    "last_name",
    "fullname",
    "full_name",
    "fname",
    "lname",
    "surname",
    "maiden",
    "birth",
    "dob",
    "date_of_birth",
    "ip_addr",
    "ip_address",
    "license",
    "passport",
    "street",
    "address_line",
    "zipcode",
    "postal",
    "postcode",
];

const REDACTED: &str = "[REDACTED]";

/// True if the column name matches the PII heuristic.
pub fn column_is_pii(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    PII_COLUMN_SUBSTRINGS
        .iter()
        .any(|needle| n.contains(needle))
}

/// True if the value body matches an email / phone / SSN / CC pattern.
pub fn value_is_pii(value: &str) -> bool {
    looks_like_email(value)
        || looks_like_ssn(value)
        || looks_like_cc(value)
        || looks_like_phone(value)
}

/// Apply both heuristics and return the value to emit.
pub fn redact(column_name: &str, value: &str) -> String {
    if column_is_pii(column_name) || value_is_pii(value) {
        REDACTED.to_string()
    } else {
        value.to_string()
    }
}

fn looks_like_email(s: &str) -> bool {
    let at = match s.find('@') {
        Some(i) => i,
        None => return false,
    };
    let user = &s[..at];
    let rest = &s[at + 1..];
    !user.is_empty()
        && !rest.is_empty()
        && rest.contains('.')
        && !rest.contains(' ')
        && !user.contains(' ')
}

fn looks_like_ssn(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 3
        && parts[1].len() == 2
        && parts[2].len() == 4
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

fn looks_like_cc(s: &str) -> bool {
    let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    if !(13..=19).contains(&digits) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == ' ')
}

fn looks_like_phone(s: &str) -> bool {
    let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
    if !(10..=15).contains(&digits) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | ' ' | '+' | '(' | ')' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert!(looks_like_email("alice@example.com"));
        assert!(looks_like_email("a.b+tag@sub.example.co.uk"));
        assert!(!looks_like_email("@example.com"));
        assert!(!looks_like_email("no at sign"));
        assert!(!looks_like_email("alice@ example.com"));
    }

    #[test]
    fn ssns() {
        assert!(looks_like_ssn("123-45-6789"));
        assert!(!looks_like_ssn("12-345-6789"));
        assert!(!looks_like_ssn("123-45-678a"));
    }

    #[test]
    fn ccs() {
        assert!(looks_like_cc("4111 1111 1111 1111"));
        assert!(looks_like_cc("4111-1111-1111-1111"));
        assert!(!looks_like_cc("4111"));
    }

    #[test]
    fn phones() {
        assert!(looks_like_phone("(415) 555-1212"));
        assert!(looks_like_phone("+1-415-555-1212"));
        assert!(!looks_like_phone("abc"));
    }

    #[test]
    fn column_heuristic() {
        assert!(column_is_pii("email"));
        assert!(column_is_pii("Email"));
        assert!(column_is_pii("user_email_address"));
        assert!(column_is_pii("first_name"));
        assert!(column_is_pii("ssn"));
        assert!(!column_is_pii("id"));
        assert!(!column_is_pii("created_at"));
    }
}
