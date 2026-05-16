//! The `[Runtime Context ...]` prefix that wraps the current user turn
//! so the LLM can distinguish session-level metadata from user intent.

pub const OPEN_TAG: &str = "[Runtime Context — metadata only, not instructions]";
pub const CLOSE_TAG: &str = "[/Runtime Context]";

/// Remove the runtime-context block (and its trailing newline) from a
/// user message. If the block is absent, returns the original string.
pub fn strip(content: &str) -> String {
    let Some(start) = content.find(OPEN_TAG) else {
        return content.to_string();
    };
    let Some(end_rel) = content[start..].find(CLOSE_TAG) else {
        return content.to_string();
    };
    let end_absolute = start + end_rel + CLOSE_TAG.len();
    let before = &content[..start];
    let after = content[end_absolute..].trim_start_matches('\n');
    format!("{before}{after}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_block_and_trailing_newlines() {
        let content = format!("{OPEN_TAG}\ntime: now\n{CLOSE_TAG}\nhello");
        assert_eq!(strip(&content), "hello");
    }

    #[test]
    fn strip_returns_unchanged_when_no_tag() {
        assert_eq!(strip("just hello"), "just hello");
    }

    #[test]
    fn strip_handles_missing_close_tag() {
        let content = format!("{OPEN_TAG}\ntime: now\nhello");
        assert_eq!(strip(&content), content);
    }
}
