use ash_core::*;
use insta::assert_json_snapshot;

#[test]
fn serializes_user_message_with_the_stable_contract() {
    let msg = Message::user("Hello, world!");
    assert_json_snapshot!("user_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn serializes_assistant_message_with_the_stable_contract() {
    let msg = Message::assistant_text("I can help with that.");
    assert_json_snapshot!("assistant_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn serializes_system_message_with_the_stable_contract() {
    let msg = Message::system("You are a helpful assistant.");
    assert_json_snapshot!("system_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn serializes_tool_call_block_with_the_stable_contract() {
    let block = ContentBlock::ToolCall {
        id: ToolCallId::new(),
        name: "bash".to_string(),
        arguments: serde_json::json!({"command": "ls -la"}),
    };
    assert_json_snapshot!("tool_call_block", block, {
        ".id" => "[uuid]",
    });
}

#[test]
fn serializes_tool_result_message_with_the_stable_contract() {
    let msg = Message::tool_result(
        ToolCallId::new(),
        Ok("file1.txt\nfile2.txt".to_string()),
        Vec::new(),
    );
    assert_json_snapshot!("tool_result_message", msg, {
        ".id" => "[uuid]",
        ".content.id" => "[uuid]",
    });
}

#[test]
fn serializes_tool_result_error_with_the_stable_contract() {
    let msg = Message::tool_result(
        ToolCallId::new(),
        Err("command not found".to_string()),
        Vec::new(),
    );
    assert_json_snapshot!("tool_result_error", msg, {
        ".id" => "[uuid]",
        ".content.id" => "[uuid]",
    });
}

#[test]
fn serializes_image_content_with_the_stable_contract() {
    let content = Content::Image {
        media_type: "image/png".to_string(),
        data: vec![0x89, 0x50, 0x4E, 0x47],
    };
    assert_json_snapshot!("image_content", content);
}

#[test]
fn serializes_text_content_with_the_stable_contract() {
    let content = Content::Text("Hello".to_string());
    assert_json_snapshot!("text_content", content);
}

#[test]
fn serializes_event_text_delta_with_the_stable_contract() {
    let event = SessionEventKind::Live(LiveEvent::TextDelta("Hello".to_string()));
    assert_json_snapshot!("event_text_delta", event);
}

#[test]
fn serializes_event_envelope_with_the_stable_contract() {
    let event = SessionEvent {
        session_id: SessionId::new(),
        turn_id: Some(TurnId::new()),
        sequence: 3,
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-07-31T06:15:28Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        kind: SessionEventKind::Live(LiveEvent::TextDelta("Hello".to_string())),
    };
    assert_json_snapshot!("event_envelope", event, {
        ".session_id" => "[uuid]",
        ".turn_id" => "[uuid]",
    });
}

#[test]
fn serializes_event_tool_call_start_with_the_stable_contract() {
    let event = SessionEventKind::Live(LiveEvent::ToolStarted {
        id: ToolCallId::new(),
        name: "read".to_string(),
        arguments: serde_json::json!({"path": "README.md"}),
    });
    assert_json_snapshot!("event_tool_call_start", event, {
        ".id" => "[uuid]",
    });
}

#[test]
fn serializes_event_usage_with_the_stable_contract() {
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 50,
        tool_calls: 2,
    };
    assert_json_snapshot!("event_usage", usage);
}

#[test]
fn serializes_event_context_compacted_with_the_stable_contract() {
    let event = SessionEventKind::ContextCompacted(ContextUpdate {
        before_tokens: 180_000,
        after_tokens: 12_000,
        dropped_messages: 42,
    });
    assert_json_snapshot!("event_context_compacted", event);
}

#[test]
fn model_id_displays_its_string_form() {
    let model = ModelId::new("claude-sonnet-4-20250514");
    assert_eq!(model.to_string(), "claude-sonnet-4-20250514");
}

#[test]
fn role_displays_its_string_form() {
    // Display shares the lowercase vocabulary with serde and `FromStr`, so
    // the three representations can never diverge.
    assert_eq!(Role::User.to_string(), "user");
    assert_eq!(Role::Assistant.to_string(), "assistant");
    assert_eq!(Role::System.to_string(), "system");
}

#[test]
fn role_serde_and_strum_parse_share_lowercase_vocabulary() {
    for (role, serialized) in [
        (Role::User, "user"),
        (Role::Assistant, "assistant"),
        (Role::System, "system"),
    ] {
        let json = serde_json::to_value(role).unwrap();
        assert_eq!(json, serde_json::json!(serialized));
        let parsed: Role = serialized.parse().unwrap();
        assert_eq!(parsed, role);
    }

    assert_eq!(
        serde_json::from_value::<Role>(serde_json::json!("User")).unwrap(),
        Role::User
    );
}
