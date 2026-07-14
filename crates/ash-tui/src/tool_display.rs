use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn tool_activity_summary(name: &str, arguments: &Value, max_width: u16) -> String {
    let phrase = tool_phrase(name, arguments);
    truncate_width(
        &join_parts(phrase.running, &phrase.detail),
        usize::from(max_width.max(1)),
    )
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

struct ToolPhrase {
    running: &'static str,
    completed: &'static str,
    failed: &'static str,
    detail: String,
}

fn tool_phrase(name: &str, arguments: &Value) -> ToolPhrase {
    match name {
        "read" => phrase(
            "Reading",
            "Read",
            "reading",
            path_argument(arguments, "path"),
        ),
        "write" => phrase(
            "Writing",
            "Wrote",
            "writing",
            path_argument(arguments, "path"),
        ),
        "edit" => phrase(
            "Editing",
            "Edited",
            "editing",
            path_argument(arguments, "path"),
        ),
        "grep" => phrase(
            "Searching",
            "Searched",
            "searching",
            search_detail(arguments),
        ),
        "find" => phrase("Listing", "Listed", "listing", find_detail(arguments)),
        "bash" => phrase(
            "Running",
            "Ran",
            "running",
            string_argument(arguments, "command"),
        ),
        "spawn_agent" => phrase(
            "Spawning",
            "Spawned",
            "spawning",
            string_argument(arguments, "task_name"),
        ),
        "send_message" => phrase(
            "Messaging",
            "Messaged",
            "messaging",
            string_argument(arguments, "target"),
        ),
        "followup_task" => phrase(
            "Continuing",
            "Continued",
            "continuing",
            string_argument(arguments, "target"),
        ),
        "interrupt_agent" => phrase(
            "Interrupting",
            "Interrupted",
            "interrupting",
            string_argument(arguments, "target"),
        ),
        "list_agents" => phrase("Listing", "Listed", "listing", "agents".to_string()),
        "wait_agent" => phrase("Waiting", "Waited", "waiting", "for agents".to_string()),
        _ => phrase(
            "Running",
            "Ran",
            "running",
            sanitize_single_line(name).replace('_', " "),
        ),
    }
}

fn phrase(
    running: &'static str,
    completed: &'static str,
    failed: &'static str,
    detail: String,
) -> ToolPhrase {
    ToolPhrase {
        running,
        completed,
        failed,
        detail,
    }
}

fn search_detail(arguments: &Value) -> String {
    let pattern = string_argument(arguments, "pattern");
    append_short_path(pattern, arguments, "path")
}

fn find_detail(arguments: &Value) -> String {
    let pattern = string_argument(arguments, "pattern");
    append_short_path(pattern, arguments, "path")
}

fn append_short_path(mut detail: String, arguments: &Value, key: &str) -> String {
    let Some(path) = arguments.get(key).and_then(Value::as_str) else {
        return detail;
    };
    let path = path.trim();
    if path.is_empty() || path == "." {
        return detail;
    }
    let path = short_display_path(path);
    if detail.is_empty() {
        path
    } else {
        detail.push_str(" in ");
        detail.push_str(&path);
        detail
    }
}

fn path_argument(arguments: &Value, key: &str) -> String {
    arguments
        .get(key)
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

fn short_display_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .split('/')
        .rev()
        .find(|part| {
            !part.is_empty() && !matches!(*part, "build" | "dist" | "node_modules" | "src")
        })
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
    let action = truncate_width(action, max_width);
    if detail.is_empty() {
        return (action, String::new());
    }
    let remaining = max_width
        .saturating_sub(UnicodeWidthStr::width(action.as_str()))
        .saturating_sub(1);
    if remaining == 0 {
        (action, String::new())
    } else {
        (action, truncate_width(detail, remaining))
    }
}

fn truncate_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let mut output = String::new();
    let mut width = 0;
    let content_width = max_width.saturating_sub(1);
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_builtin_tools_as_semantic_summaries() {
        assert_eq!(
            tool_call_summary(
                "bash",
                &json!({"command": "cargo test", "cwd": "/tmp/project"}),
                false,
                80,
            ),
            ("Ran".to_string(), "cargo test".to_string())
        );
        assert_eq!(
            tool_call_summary(
                "grep",
                &json!({"pattern": "reasoning", "path": "/work/ash/crates/ash-tui/src"}),
                false,
                80,
            ),
            ("Searched".to_string(), "reasoning in ash-tui".to_string())
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
