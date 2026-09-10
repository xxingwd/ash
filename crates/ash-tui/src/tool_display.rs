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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolGrouping {
    Never,
    Always,
    Collapsed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolDisplay {
    pub renderer: ToolRenderer,
    pub grouping: ToolGrouping,
}

impl ToolDisplay {
    const fn generic(output: OutputPresentation, grouping: ToolGrouping) -> Self {
        Self {
            renderer: ToolRenderer::Generic(output),
            grouping,
        }
    }
}

const GENERIC_PREVIEW: ToolDisplay =
    ToolDisplay::generic(OutputPresentation::Preview, ToolGrouping::Never);
const GENERIC_SUMMARY: ToolDisplay =
    ToolDisplay::generic(OutputPresentation::Summary, ToolGrouping::Never);
const GENERIC_EXPANDABLE: ToolDisplay =
    ToolDisplay::generic(OutputPresentation::Expandable, ToolGrouping::Collapsed);

pub fn tool_display_for(name: &str) -> ToolDisplay {
    match ToolKind::from_name(name) {
        ToolKind::Bash => ToolDisplay {
            renderer: ToolRenderer::Bash,
            grouping: ToolGrouping::Never,
        },
        ToolKind::Edit => ToolDisplay {
            renderer: ToolRenderer::Edit,
            grouping: ToolGrouping::Never,
        },
        ToolKind::Write => ToolDisplay {
            renderer: ToolRenderer::Write,
            grouping: ToolGrouping::Never,
        },
        ToolKind::Read => ToolDisplay::generic(OutputPresentation::Omitted, ToolGrouping::Always),
        ToolKind::Glob | ToolKind::Grep | ToolKind::WebFetch => GENERIC_SUMMARY,
        ToolKind::Skill
        | ToolKind::Agent
        | ToolKind::MessageAgent
        | ToolKind::ListAgents
        | ToolKind::RemoveAgent
        | ToolKind::WaitAgent => GENERIC_EXPANDABLE,
        ToolKind::Other => GENERIC_PREVIEW,
    }
}

pub fn tool_display_for_result(name: &str, is_error: bool) -> ToolDisplay {
    if is_error {
        GENERIC_PREVIEW
    } else {
        tool_display_for(name)
    }
}

/// One tool call's title label and detail. The label is the exact tool
/// name; presentation (label casing, state colors) happens at the render
/// layer.
pub fn tool_call_summary(name: &str, arguments: &Value) -> (String, String) {
    (name.to_string(), tool_detail(name, arguments))
}

/// Consecutive same-name tools join a group only when this tool's display
/// policy says so. Failed tools never group, regardless of this setting.
pub fn is_groupable_tool(name: &str, expanded: bool) -> bool {
    match tool_display_for(name).grouping {
        ToolGrouping::Always => true,
        ToolGrouping::Collapsed => !expanded,
        ToolGrouping::Never => false,
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
    fn display_policy_is_centralized_by_tool_kind() {
        assert_eq!(
            tool_display_for("bash"),
            ToolDisplay {
                renderer: ToolRenderer::Bash,
                grouping: ToolGrouping::Never,
            }
        );
        assert_eq!(
            tool_display_for("edit"),
            ToolDisplay {
                renderer: ToolRenderer::Edit,
                grouping: ToolGrouping::Never,
            }
        );
        assert_eq!(
            tool_display_for("write"),
            ToolDisplay {
                renderer: ToolRenderer::Write,
                grouping: ToolGrouping::Never,
            }
        );
        assert_eq!(
            tool_display_for("read"),
            ToolDisplay::generic(OutputPresentation::Omitted, ToolGrouping::Always)
        );
        for name in ["grep", "glob", "webfetch"] {
            assert_eq!(tool_display_for(name), GENERIC_SUMMARY, "{name}");
        }
        for name in [
            "skill",
            "agent",
            "wait_agent",
            "list_agents",
            "remove_agent",
        ] {
            assert_eq!(tool_display_for(name), GENERIC_EXPANDABLE, "{name}");
        }
        assert_eq!(tool_display_for("custom_tool"), GENERIC_PREVIEW);
        assert_eq!(tool_display_for_result("read", true), GENERIC_PREVIEW);
        assert_eq!(tool_display_for_result("wait_agent", true), GENERIC_PREVIEW);
        assert_eq!(tool_display_for_result("bash", true), GENERIC_PREVIEW);
    }

    #[test]
    fn workspace_path_strips_the_working_directory_prefix() {
        use std::path::Path;
        let root = Path::new("/home/user/workspace/project");
        assert_eq!(
            workspace_path("/home/user/workspace/project/docs/design.md", root,),
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
