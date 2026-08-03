use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::text_width::truncate_end;

const GROUP_DETAIL_LIMIT: usize = 4;

pub(crate) fn read_group_detail(name: &str, arguments: &Value) -> Option<String> {
    (name == "read").then(|| path_argument(arguments))
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
    match name {
        "read" => phrase("Read", "reading", path_argument(arguments)),
        "write" => phrase("Wrote", "writing", path_argument(arguments)),
        "edit" => phrase("Edited", "editing", path_argument(arguments)),
        "glob" => phrase("Found", "finding", string_argument(arguments, "pattern")),
        "grep" => phrase(
            "Searched",
            "searching",
            string_argument(arguments, "pattern"),
        ),
        "webfetch" => phrase("Fetched", "fetching", url_argument(arguments)),
        "bash" => phrase("Ran", "running", string_argument(arguments, "command")),
        "skill" => phrase("Loaded", "loading", string_argument(arguments, "name")),
        "spawn_agent" => phrase(
            "Spawned",
            "spawning",
            string_argument(arguments, "task_name"),
        ),
        "send_message" => phrase(
            "Messaged",
            "messaging",
            string_argument(arguments, "target"),
        ),
        "followup_task" => phrase(
            "Continued",
            "continuing",
            string_argument(arguments, "target"),
        ),
        "interrupt_agent" => phrase(
            "Interrupted",
            "interrupting",
            string_argument(arguments, "target"),
        ),
        "list_agents" => phrase("Listed", "listing", "agents".to_string()),
        "wait_agent" => phrase("Waited", "waiting", "for agents".to_string()),
        _ => phrase(
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
}
