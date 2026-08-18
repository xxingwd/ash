use std::fmt::Write;

use serde_json::Value;

use crate::scrollback::{sanitize_single_line, sanitize_terminal_text};

const GROUP_DETAIL_LIMIT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolKind {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    WebFetch,
    Bash,
    Skill,
    Agent,
    MessageAgent,
    WaitAgent,
    Other,
}

impl ToolKind {
    fn from_name(name: &str) -> Self {
        match name {
            "read" => Self::Read,
            "write" => Self::Write,
            "edit" => Self::Edit,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "webfetch" => Self::WebFetch,
            "bash" => Self::Bash,
            "skill" => Self::Skill,
            "agent" => Self::Agent,
            "message_agent" => Self::MessageAgent,
            "wait_agent" => Self::WaitAgent,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRenderer {
    Bash,
    Edit,
    Write,
    Generic(OutputPresentation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputPresentation {
    Hidden,
    Summary,
    Preview,
}

pub fn tool_renderer(name: &str, is_error: bool) -> ToolRenderer {
    let kind = ToolKind::from_name(name);
    match (kind, is_error) {
        (ToolKind::Bash, _) => ToolRenderer::Bash,
        (ToolKind::Edit, false) => ToolRenderer::Edit,
        (ToolKind::Write, false) => ToolRenderer::Write,
        (_, true) => ToolRenderer::Generic(OutputPresentation::Preview),
        (ToolKind::Glob | ToolKind::Grep, false) => {
            ToolRenderer::Generic(OutputPresentation::Summary)
        }
        (
            ToolKind::Read
            | ToolKind::Skill
            | ToolKind::Agent
            | ToolKind::MessageAgent
            | ToolKind::WaitAgent,
            false,
        ) => ToolRenderer::Generic(OutputPresentation::Hidden),
        _ => ToolRenderer::Generic(OutputPresentation::Preview),
    }
}

pub fn read_group_detail(name: &str, arguments: &Value) -> Option<String> {
    (ToolKind::from_name(name) == ToolKind::Read).then(|| short_path_argument(arguments))
}

/// One tool call's title label and detail. The label is the exact tool
/// name; presentation (label casing, state colors) happens at the render
/// layer.
pub fn tool_call_summary(name: &str, arguments: &Value) -> (String, String) {
    (name.to_string(), tool_detail(name, arguments))
}

fn tool_detail(name: &str, arguments: &Value) -> String {
    match ToolKind::from_name(name) {
        ToolKind::Read => short_path_argument(arguments),
        ToolKind::Write | ToolKind::Edit => raw_path_argument(arguments),
        ToolKind::Glob | ToolKind::Grep => string_argument(arguments, "pattern"),
        ToolKind::WebFetch => url_argument(arguments),
        ToolKind::Bash => string_argument(arguments, "command"),
        ToolKind::Skill => string_argument(arguments, "name"),
        ToolKind::Agent | ToolKind::MessageAgent => string_argument(arguments, "name"),
        ToolKind::WaitAgent | ToolKind::Other => String::new(),
    }
}

pub fn read_group_summary(details: &[String]) -> (String, String) {
    let mut unique = details.iter().filter(|detail| !detail.is_empty()).fold(
        Vec::new(),
        |mut unique, detail| {
            if !unique.contains(&detail.as_str()) {
                unique.push(detail.as_str());
            }
            unique
        },
    );
    let hidden = unique.len().saturating_sub(GROUP_DETAIL_LIMIT);
    unique.truncate(GROUP_DETAIL_LIMIT);
    let mut detail = unique.join(", ");
    if hidden > 0 {
        let _ = write!(detail, " +{hidden}");
    }
    ("read".to_string(), detail)
}

fn raw_path_argument(arguments: &Value) -> String {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(sanitize_single_line)
        .unwrap_or_default()
}

fn short_path_argument(arguments: &Value) -> String {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(display_path)
        .unwrap_or_default()
}

fn string_argument(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(collapse_whitespace)
        .unwrap_or_default()
}

fn url_argument(arguments: &Value) -> String {
    let url = string_argument(arguments, "url");
    let without_fragment = url.split_once('#').map_or(url.as_str(), |(head, _)| head);
    let without_query = without_fragment
        .split_once('?')
        .map_or(without_fragment, |(head, _)| head);
    let Some((scheme, rest)) = without_query.split_once("://") else {
        return without_query.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

pub(crate) fn display_path(path: &str) -> String {
    let normalized = sanitize_single_line(path).replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .split('/')
        .rev()
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// Collapse runs of whitespace to single spaces (unlike
/// `scrollback::sanitize_single_line`, which only replaces newlines).
fn collapse_whitespace(value: &str) -> String {
    sanitize_terminal_text(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_tools_by_name_with_argument_details() {
        assert_eq!(
            tool_call_summary("bash", &json!({"command": "cargo test"})),
            ("bash".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            tool_call_summary(
                "bash",
                &json!({"command": "rg reasoning /work/ash/crates/ash-tui/src"}),
            ),
            (
                "bash".to_string(),
                "rg reasoning /work/ash/crates/ash-tui/src".to_string()
            )
        );
        assert_eq!(
            tool_call_summary(
                "webfetch",
                &json!({"url": "https://user:secret@example.com/docs?q=token#section"}),
            ),
            (
                "webfetch".to_string(),
                "https://example.com/docs".to_string()
            )
        );
        assert_eq!(
            tool_call_summary("glob", &json!({"pattern": "**/*.rs"})),
            ("glob".to_string(), "**/*.rs".to_string())
        );
        assert_eq!(
            tool_call_summary("grep", &json!({"pattern": "TODO|FIXME"})),
            ("grep".to_string(), "TODO|FIXME".to_string())
        );
        assert_eq!(
            tool_call_summary("skill", &json!({"name": "review"})),
            ("skill".to_string(), "review".to_string())
        );
    }

    #[test]
    fn collaboration_tool_names_read_as_labels() {
        assert_eq!(
            tool_call_summary(
                "message_agent",
                &json!({"name": "research", "message": "hi", "wait": false}),
            ),
            ("message_agent".to_string(), "research".to_string())
        );
        assert_eq!(
            tool_call_summary("custom_tool", &json!({"payload": "x"})),
            ("custom_tool".to_string(), String::new())
        );
    }

    #[test]
    fn preserves_edit_and_write_paths_but_shortens_reads() {
        let edit = tool_call_summary(
            "edit",
            &json!({
                "path": "/home/user/work/ash/crates/ash-tui/src/inline.rs",
                "old": "many lines of old content",
                "new": "many lines of new content"
            }),
        );
        let write = tool_call_summary(
            "write",
            &json!({"path": "crates/ash-tui/src/new.rs", "content": "content"}),
        );
        let read = tool_call_summary(
            "read",
            &json!({"path": "/home/user/work/ash/crates/ash-tui/src/inline.rs"}),
        );

        assert_eq!(
            edit,
            (
                "edit".to_string(),
                "/home/user/work/ash/crates/ash-tui/src/inline.rs".to_string()
            )
        );
        assert_eq!(
            write,
            ("write".to_string(), "crates/ash-tui/src/new.rs".to_string())
        );
        assert_eq!(read, ("read".to_string(), "inline.rs".to_string()));
    }

    #[test]
    fn groups_matching_tools_behind_one_action() {
        assert_eq!(
            read_group_summary(&["inline.rs".to_string(), "viewport.rs".to_string()]),
            ("read".to_string(), "inline.rs, viewport.rs".to_string())
        );
    }

    #[test]
    fn group_summaries_cap_visible_details() {
        let details = ["a", "b", "c", "d", "e", "f"].map(str::to_string);

        assert_eq!(
            read_group_summary(&details),
            ("read".to_string(), "a, b, c, d +2".to_string())
        );
    }

    #[test]
    fn group_summaries_deduplicate_details() {
        assert_eq!(
            read_group_summary(&["app.rs".to_string(), "app.rs".to_string()]),
            ("read".to_string(), "app.rs".to_string())
        );
    }

    #[test]
    fn renderer_policy_is_centralized_by_tool_kind() {
        assert_eq!(tool_renderer("bash", false), ToolRenderer::Bash);
        assert_eq!(tool_renderer("edit", false), ToolRenderer::Edit);
        assert_eq!(tool_renderer("write", false), ToolRenderer::Write);
        assert_eq!(
            tool_renderer("read", false),
            ToolRenderer::Generic(OutputPresentation::Hidden)
        );
        assert_eq!(
            tool_renderer("grep", false),
            ToolRenderer::Generic(OutputPresentation::Summary)
        );
        assert_eq!(
            tool_renderer("glob", false),
            ToolRenderer::Generic(OutputPresentation::Summary)
        );
        assert_eq!(
            tool_renderer("skill", false),
            ToolRenderer::Generic(OutputPresentation::Hidden)
        );
        assert_eq!(
            tool_renderer("agent", false),
            ToolRenderer::Generic(OutputPresentation::Hidden)
        );
        assert_eq!(
            tool_renderer("wait_agent", false),
            ToolRenderer::Generic(OutputPresentation::Hidden)
        );
        assert_eq!(
            tool_renderer("read", true),
            ToolRenderer::Generic(OutputPresentation::Preview)
        );
        assert_eq!(
            tool_renderer("wait_agent", true),
            ToolRenderer::Generic(OutputPresentation::Preview)
        );
        assert_eq!(
            tool_renderer("custom_tool", false),
            ToolRenderer::Generic(OutputPresentation::Preview)
        );
    }
}
