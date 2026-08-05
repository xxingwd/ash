use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::text_width::truncate_end;

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
            "message_agent" | "send_message" | "followup_task" => Self::MessageAgent,
            "interrupt_agent" => Self::InterruptAgent,
            "wait_agent" | "list_agents" => Self::WaitAgent,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChangePreviewSource {
    EditOutput,
    WriteContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolRenderer {
    Bash,
    ChangePreview(ChangePreviewSource),
    Generic { show_output: bool },
}

pub(crate) fn tool_renderer(name: &str, is_error: bool) -> ToolRenderer {
    let kind = ToolKind::from_name(name);
    match (kind, is_error) {
        (ToolKind::Bash, _) => ToolRenderer::Bash,
        (ToolKind::Edit, false) => ToolRenderer::ChangePreview(ChangePreviewSource::EditOutput),
        (ToolKind::Write, false) => ToolRenderer::ChangePreview(ChangePreviewSource::WriteContent),
        (ToolKind::Read | ToolKind::Edit | ToolKind::Write, _) => {
            ToolRenderer::Generic { show_output: false }
        }
        _ => ToolRenderer::Generic { show_output: true },
    }
}

pub(crate) fn read_group_detail(name: &str, arguments: &Value) -> Option<String> {
    (ToolKind::from_name(name) == ToolKind::Read).then(|| path_argument(arguments))
}

pub(crate) fn tool_call_summary(
    name: &str,
    arguments: &Value,
    is_error: bool,
    max_width: u16,
) -> (String, String) {
    let phrase = tool_phrase(name, arguments);
    let (action, detail) = if is_error {
        ("Failed", join_parts(phrase.failed, &phrase.detail))
    } else {
        (phrase.completed, phrase.detail)
    };
    fit_action_and_detail(action, &detail, usize::from(max_width.max(1)))
}

pub(crate) fn read_group_summary(details: &[String], max_width: u16) -> (String, String) {
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
        detail.push_str(&format!(" +{hidden}"));
    }
    fit_action_and_detail("Read", &detail, usize::from(max_width.max(1)))
}

struct ToolPhrase {
    completed: &'static str,
    failed: &'static str,
    detail: String,
}

fn tool_phrase(name: &str, arguments: &Value) -> ToolPhrase {
    match ToolKind::from_name(name) {
        ToolKind::Read => phrase("Read", "reading", path_argument(arguments)),
        ToolKind::Write => phrase("Wrote", "writing", path_argument(arguments)),
        ToolKind::Edit => phrase("Edited", "editing", path_argument(arguments)),
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
            sanitize_single_line(name).replace('_', " "),
        ),
    }
}

fn phrase(completed: &'static str, failed: &'static str, detail: String) -> ToolPhrase {
    ToolPhrase {
        completed,
        failed,
        detail,
    }
}

fn path_argument(arguments: &Value) -> String {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(short_display_path)
        .unwrap_or_default()
}

fn string_argument(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(sanitize_single_line)
        .unwrap_or_default()
}

fn url_argument(arguments: &Value) -> String {
    let url = string_argument(arguments, "url");
    let without_fragment = url.split('#').next().unwrap_or(&url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
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

fn short_display_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .split('/')
        .rev()
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn sanitize_single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn join_parts(action: &str, detail: &str) -> String {
    if detail.is_empty() {
        action.to_string()
    } else {
        format!("{action} {detail}")
    }
}

fn fit_action_and_detail(action: &str, detail: &str, max_width: usize) -> (String, String) {
    let action = truncate_end(action, max_width);
    if detail.is_empty() {
        return (action, String::new());
    }
    let remaining = max_width
        .saturating_sub(UnicodeWidthStr::width(action.as_str()))
        .saturating_sub(1);
    if remaining == 0 {
        (action, String::new())
    } else {
        (action, truncate_end(detail, remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_builtin_tools_as_semantic_summaries() {
        assert_eq!(
            tool_call_summary("bash", &json!({"command": "cargo test"}), false, 80,),
            ("Ran".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            tool_call_summary(
                "bash",
                &json!({"command": "rg reasoning /work/ash/crates/ash-tui/src"}),
                false,
                80,
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
                80,
            ),
            (
                "Fetched".to_string(),
                "https://example.com/docs".to_string()
            )
        );
        assert_eq!(
            tool_call_summary("glob", &json!({"pattern": "**/*.rs"}), false, 80),
            ("Found".to_string(), "**/*.rs".to_string())
        );
        assert_eq!(
            tool_call_summary("grep", &json!({"pattern": "TODO|FIXME"}), false, 80),
            ("Searched".to_string(), "TODO|FIXME".to_string())
        );
        assert_eq!(
            tool_call_summary("skill", &json!({"name": "review"}), false, 80),
            ("Loaded".to_string(), "review".to_string())
        );
    }

    #[test]
    fn hides_edit_payloads_and_absolute_paths() {
        let summary = tool_call_summary(
            "edit",
            &json!({
                "path": "/home/user/work/ash/crates/ash-tui/src/inline.rs",
                "old": "many lines of old content",
                "new": "many lines of new content"
            }),
            false,
            80,
        );

        assert_eq!(summary, ("Edited".to_string(), "inline.rs".to_string()));
    }

    #[test]
    fn renders_failed_calls_without_dumping_arguments() {
        assert_eq!(
            tool_call_summary(
                "write",
                &json!({"path": "/tmp/report.md", "content": "one\ntwo"}),
                true,
                80,
            ),
            ("Failed".to_string(), "writing report.md".to_string())
        );
    }

    #[test]
    fn groups_matching_tools_behind_one_action() {
        assert_eq!(
            read_group_summary(&["inline.rs".to_string(), "viewport.rs".to_string()], 80,),
            ("Read".to_string(), "inline.rs, viewport.rs".to_string())
        );
    }

    #[test]
    fn group_summaries_cap_visible_details() {
        let details = ["a", "b", "c", "d", "e", "f"].map(str::to_string);

        assert_eq!(
            read_group_summary(&details, 80),
            ("Read".to_string(), "a, b, c, d +2".to_string())
        );
    }

    #[test]
    fn group_summaries_deduplicate_details() {
        assert_eq!(
            read_group_summary(&["app.rs".to_string(), "app.rs".to_string()], 80,),
            ("Read".to_string(), "app.rs".to_string())
        );
    }

    #[test]
    fn truncates_the_detail_to_the_available_width() {
        let (action, detail) = tool_call_summary(
            "bash",
            &json!({"command": "cargo test --workspace --all-targets"}),
            false,
            20,
        );
        let rendered = join_parts(&action, &detail);

        assert!(UnicodeWidthStr::width(rendered.as_str()) <= 20);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn renderer_policy_is_centralized_by_tool_kind() {
        assert_eq!(tool_renderer("bash", false), ToolRenderer::Bash);
        assert_eq!(
            tool_renderer("edit", false),
            ToolRenderer::ChangePreview(ChangePreviewSource::EditOutput)
        );
        assert_eq!(
            tool_renderer("read", true),
            ToolRenderer::Generic { show_output: false }
        );
        assert_eq!(
            tool_renderer("custom_tool", false),
            ToolRenderer::Generic { show_output: true }
        );
    }
}
