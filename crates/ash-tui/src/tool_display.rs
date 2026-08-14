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
    SpawnAgent,
    MessageAgent,
    InterruptAgent,
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
            "spawn_agent" => Self::SpawnAgent,
            "message_agent" => Self::MessageAgent,
            "interrupt_agent" => Self::InterruptAgent,
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
    Generic { show_output: bool },
}

pub fn tool_renderer(name: &str, is_error: bool) -> ToolRenderer {
    let kind = ToolKind::from_name(name);
    match (kind, is_error) {
        (ToolKind::Bash, _) => ToolRenderer::Bash,
        (_, true) => ToolRenderer::Generic { show_output: true },
        (ToolKind::Edit, false) => ToolRenderer::Edit,
        (ToolKind::Write, false) => ToolRenderer::Write,
        (ToolKind::Read, false) => ToolRenderer::Generic { show_output: false },
        _ => ToolRenderer::Generic { show_output: true },
    }
}

pub fn read_group_detail(name: &str, arguments: &Value) -> Option<String> {
    (ToolKind::from_name(name) == ToolKind::Read).then(|| short_path_argument(arguments))
}

pub fn tool_call_summary(name: &str, arguments: &Value, is_error: bool) -> (String, String) {
    let phrase = tool_phrase(name, arguments);
    let (action, detail) = if is_error {
        ("Failed", join_parts(phrase.running, &phrase.detail))
    } else {
        (phrase.completed, phrase.detail)
    };
    (action.to_string(), detail)
}

pub fn running_tool_call_summary(name: &str, arguments: &Value) -> (String, String) {
    let phrase = tool_phrase(name, arguments);
    (sentence_case(phrase.running), phrase.detail)
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
    ("Read".to_string(), detail)
}

struct ToolPhrase {
    completed: &'static str,
    running: &'static str,
    detail: String,
}

fn tool_phrase(name: &str, arguments: &Value) -> ToolPhrase {
    match ToolKind::from_name(name) {
        ToolKind::Read => phrase("Read", "reading", short_path_argument(arguments)),
        ToolKind::Write => phrase("Wrote", "writing", raw_path_argument(arguments)),
        ToolKind::Edit => phrase("Edited", "editing", raw_path_argument(arguments)),
        ToolKind::Glob => phrase("Found", "finding", string_argument(arguments, "pattern")),
        ToolKind::Grep => phrase(
            "Searched",
            "searching",
            string_argument(arguments, "pattern"),
        ),
        ToolKind::WebFetch => phrase("Fetched", "fetching", url_argument(arguments)),
        ToolKind::Bash => phrase("Ran", "running", string_argument(arguments, "command")),
        ToolKind::Skill => phrase("Loaded", "loading", string_argument(arguments, "name")),
        ToolKind::SpawnAgent => phrase(
            "Spawned",
            "spawning",
            string_argument(arguments, "task_name"),
        ),
        ToolKind::MessageAgent => {
            if arguments
                .get("start_turn")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                phrase(
                    "Continued",
                    "continuing",
                    string_argument(arguments, "target"),
                )
            } else {
                phrase(
                    "Messaged",
                    "messaging",
                    string_argument(arguments, "target"),
                )
            }
        }
        ToolKind::InterruptAgent => phrase(
            "Interrupted",
            "interrupting",
            string_argument(arguments, "target"),
        ),
        ToolKind::WaitAgent => phrase("Waited", "waiting", "for agents".to_string()),
        ToolKind::Other => phrase(
            "Ran",
            "running",
            collapse_whitespace(name).replace('_', " "),
        ),
    }
}

const fn phrase(completed: &'static str, running: &'static str, detail: String) -> ToolPhrase {
    ToolPhrase {
        completed,
        running,
        detail,
    }
}

fn sentence_case(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
    })
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

fn join_parts(action: &str, detail: &str) -> String {
    if detail.is_empty() {
        action.to_string()
    } else {
        format!("{action} {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_builtin_tools_as_semantic_summaries() {
        assert_eq!(
            tool_call_summary("bash", &json!({"command": "cargo test"}), false),
            ("Ran".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            tool_call_summary(
                "bash",
                &json!({"command": "rg reasoning /work/ash/crates/ash-tui/src"}),
                false,
            ),
            (
                "Ran".to_string(),
                "rg reasoning /work/ash/crates/ash-tui/src".to_string()
            )
        );
        assert_eq!(
            tool_call_summary(
                "webfetch",
                &json!({"url": "https://user:secret@example.com/docs?q=token#section"}),
                false,
            ),
            (
                "Fetched".to_string(),
                "https://example.com/docs".to_string()
            )
        );
        assert_eq!(
            tool_call_summary("glob", &json!({"pattern": "**/*.rs"}), false),
            ("Found".to_string(), "**/*.rs".to_string())
        );
        assert_eq!(
            tool_call_summary("grep", &json!({"pattern": "TODO|FIXME"}), false),
            ("Searched".to_string(), "TODO|FIXME".to_string())
        );
        assert_eq!(
            tool_call_summary("skill", &json!({"name": "review"}), false),
            ("Loaded".to_string(), "review".to_string())
        );
    }

    #[test]
    fn renders_running_calls_in_the_present_tense() {
        assert_eq!(
            running_tool_call_summary("bash", &json!({"command": "cargo test"})),
            ("Running".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            running_tool_call_summary("read", &json!({"path": "/work/ash/src/main.rs"})),
            ("Reading".to_string(), "main.rs".to_string())
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
            false,
        );
        let write = tool_call_summary(
            "write",
            &json!({"path": "crates/ash-tui/src/new.rs", "content": "content"}),
            false,
        );
        let read = tool_call_summary(
            "read",
            &json!({"path": "/home/user/work/ash/crates/ash-tui/src/inline.rs"}),
            false,
        );

        assert_eq!(
            edit,
            (
                "Edited".to_string(),
                "/home/user/work/ash/crates/ash-tui/src/inline.rs".to_string()
            )
        );
        assert_eq!(
            write,
            ("Wrote".to_string(), "crates/ash-tui/src/new.rs".to_string())
        );
        assert_eq!(read, ("Read".to_string(), "inline.rs".to_string()));
    }

    #[test]
    fn renders_failed_calls_without_dumping_arguments() {
        assert_eq!(
            tool_call_summary(
                "write",
                &json!({"path": "/tmp/report.md", "content": "one\ntwo"}),
                true,
            ),
            ("Failed".to_string(), "writing /tmp/report.md".to_string())
        );
    }

    #[test]
    fn groups_matching_tools_behind_one_action() {
        assert_eq!(
            read_group_summary(&["inline.rs".to_string(), "viewport.rs".to_string()]),
            ("Read".to_string(), "inline.rs, viewport.rs".to_string())
        );
    }

    #[test]
    fn group_summaries_cap_visible_details() {
        let details = ["a", "b", "c", "d", "e", "f"].map(str::to_string);

        assert_eq!(
            read_group_summary(&details),
            ("Read".to_string(), "a, b, c, d +2".to_string())
        );
    }

    #[test]
    fn group_summaries_deduplicate_details() {
        assert_eq!(
            read_group_summary(&["app.rs".to_string(), "app.rs".to_string()]),
            ("Read".to_string(), "app.rs".to_string())
        );
    }

    #[test]
    fn renderer_policy_is_centralized_by_tool_kind() {
        assert_eq!(tool_renderer("bash", false), ToolRenderer::Bash);
        assert_eq!(tool_renderer("edit", false), ToolRenderer::Edit);
        assert_eq!(tool_renderer("write", false), ToolRenderer::Write);
        assert_eq!(
            tool_renderer("read", true),
            ToolRenderer::Generic { show_output: true }
        );
        assert_eq!(
            tool_renderer("custom_tool", false),
            ToolRenderer::Generic { show_output: true }
        );
    }
}
