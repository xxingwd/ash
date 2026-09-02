use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::scrollback::{sanitize_single_line, sanitize_terminal_text};

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
    ListAgents,
    RemoveAgent,
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
            "list_agents" => Self::ListAgents,
            "remove_agent" => Self::RemoveAgent,
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
    Omitted,
    Expandable,
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
        (ToolKind::Glob | ToolKind::Grep | ToolKind::WebFetch, false) => {
            ToolRenderer::Generic(OutputPresentation::Summary)
        }
        (ToolKind::Read, false) => ToolRenderer::Generic(OutputPresentation::Omitted),
        (
            ToolKind::Skill
            | ToolKind::Agent
            | ToolKind::MessageAgent
            | ToolKind::ListAgents
            | ToolKind::RemoveAgent
            | ToolKind::WaitAgent,
            false,
        ) => ToolRenderer::Generic(OutputPresentation::Expandable),
        _ => ToolRenderer::Generic(OutputPresentation::Preview),
    }
}

/// One tool call's title label and detail. The label is the exact tool
/// name; presentation (label casing, state colors) happens at the render
/// layer.
pub fn tool_call_summary(name: &str, arguments: &Value) -> (String, String) {
    (name.to_string(), tool_detail(name, arguments))
}

/// A tool can join a consecutive same-name group exactly when its successful
/// output is not visible in the current display mode.
pub fn is_groupable_tool(name: &str, expanded: bool) -> bool {
    match tool_renderer(name, false) {
        ToolRenderer::Generic(OutputPresentation::Omitted) => true,
        ToolRenderer::Generic(OutputPresentation::Expandable) => !expanded,
        ToolRenderer::Bash
        | ToolRenderer::Edit
        | ToolRenderer::Write
        | ToolRenderer::Generic(OutputPresentation::Summary | OutputPresentation::Preview) => false,
    }
}

pub fn grouped_tool_summary(name: &str, details: &[String]) -> (String, String) {
    let mut seen = HashSet::new();
    let unique = details
        .iter()
        .filter_map(|detail| (!detail.is_empty()).then_some(detail.as_str()))
        .filter(|detail| seen.insert(*detail))
        .collect::<Vec<_>>();
    (name.to_string(), unique.join(", "))
}

fn tool_detail(name: &str, arguments: &Value) -> String {
    match ToolKind::from_name(name) {
        ToolKind::Read => short_path_argument(arguments),
        ToolKind::Write | ToolKind::Edit => raw_path_argument(arguments),
        ToolKind::Glob | ToolKind::Grep => string_argument(arguments, "pattern"),
        ToolKind::WebFetch => url_argument(arguments),
        ToolKind::Bash => string_argument(arguments, "command"),
        ToolKind::Skill => string_argument(arguments, "name"),
        ToolKind::Agent | ToolKind::MessageAgent | ToolKind::RemoveAgent => {
            string_argument(arguments, "name")
        }
        ToolKind::ListAgents | ToolKind::WaitAgent | ToolKind::Other => String::new(),
    }
}

fn raw_path_argument(arguments: &Value) -> String {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(sanitize_single_line)
        .unwrap_or_default()
}

/// Drop the workspace prefix when `path` is inside `working_dir`. Paths outside
/// the workspace stay absolute. Matching is on path components, so
/// `/tmp/workspace-other` is not treated as inside `/tmp/workspace`.
pub fn workspace_path(path: &str, working_dir: &Path) -> String {
    let normalized = sanitize_single_line(path);
    Path::new(&normalized)
        .strip_prefix(working_dir)
        .ok()
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                ".".to_string()
            } else {
                relative.to_string_lossy().replace('\\', "/")
            }
        })
        .unwrap_or_else(|| normalized.replace('\\', "/"))
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
            tool_call_summary("remove_agent", &json!({"name": "research"})),
            ("remove_agent".to_string(), "research".to_string())
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
            grouped_tool_summary(
                "read",
                &["inline.rs".to_string(), "viewport.rs".to_string()],
            ),
            ("read".to_string(), "inline.rs, viewport.rs".to_string())
        );
        assert_eq!(
            grouped_tool_summary("skill", &["review".to_string(), "explore".to_string()]),
            ("skill".to_string(), "review, explore".to_string())
        );
    }

    #[test]
    fn group_summaries_keep_every_unique_detail() {
        let details = ["a", "b", "c", "d", "e", "f"].map(str::to_string);

        assert_eq!(
            grouped_tool_summary("read", &details),
            ("read".to_string(), "a, b, c, d, e, f".to_string())
        );
    }

    #[test]
    fn group_summaries_deduplicate_details() {
        assert_eq!(
            grouped_tool_summary("read", &["app.rs".to_string(), "app.rs".to_string()]),
            ("read".to_string(), "app.rs".to_string())
        );
    }

    #[test]
    fn groupability_follows_output_visibility() {
        for name in ["read", "skill", "agent", "wait_agent"] {
            assert!(is_groupable_tool(name, false), "{name} collapsed");
        }
        assert!(is_groupable_tool("read", true));
        for name in ["skill", "agent", "wait_agent"] {
            assert!(!is_groupable_tool(name, true), "{name} expanded");
        }
        for name in ["bash", "glob", "grep", "webfetch", "write"] {
            assert!(!is_groupable_tool(name, false), "{name} collapsed");
            assert!(!is_groupable_tool(name, true), "{name} expanded");
        }
    }

    #[test]
    fn renderer_policy_is_centralized_by_tool_kind() {
        assert_eq!(tool_renderer("bash", false), ToolRenderer::Bash);
        assert_eq!(tool_renderer("edit", false), ToolRenderer::Edit);
        assert_eq!(tool_renderer("write", false), ToolRenderer::Write);
        assert_eq!(
            tool_renderer("read", false),
            ToolRenderer::Generic(OutputPresentation::Omitted)
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
            tool_renderer("webfetch", false),
            ToolRenderer::Generic(OutputPresentation::Summary)
        );
        assert_eq!(
            tool_renderer("skill", false),
            ToolRenderer::Generic(OutputPresentation::Expandable)
        );
        assert_eq!(
            tool_renderer("agent", false),
            ToolRenderer::Generic(OutputPresentation::Expandable)
        );
        assert_eq!(
            tool_renderer("wait_agent", false),
            ToolRenderer::Generic(OutputPresentation::Expandable)
        );
        assert_eq!(
            tool_renderer("list_agents", false),
            ToolRenderer::Generic(OutputPresentation::Expandable)
        );
        assert_eq!(
            tool_renderer("remove_agent", false),
            ToolRenderer::Generic(OutputPresentation::Expandable)
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

    #[test]
    fn workspace_path_strips_the_working_directory_prefix() {
        use std::path::Path;
        let root = Path::new("/home/user/workspace/project");
        assert_eq!(
            workspace_path(
                "/home/user/workspace/project/docs/design.md",
                root,
            ),
            "docs/design.md"
        );
        assert_eq!(workspace_path("/etc/hosts", root), "/etc/hosts");
        assert_eq!(
            workspace_path("/home/user/workspace/other/file.rs", root),
            "/home/user/workspace/other/file.rs"
        );
        assert_eq!(workspace_path("docs/local.md", root), "docs/local.md");
        assert_eq!(workspace_path(root.to_str().unwrap(), root), ".");
    }
}
