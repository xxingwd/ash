use ash_core::*;
use insta::assert_json_snapshot;

#[test]
fn test_user_message_snapshot() {
    let msg = Message::user("Hello, world!");
    assert_json_snapshot!("user_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn test_assistant_message_snapshot() {
    let msg = Message::assistant_text("I can help with that.");
    assert_json_snapshot!("assistant_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn test_system_message_snapshot() {
    let msg = Message::system("You are a helpful assistant.");
    assert_json_snapshot!("system_message", msg, {
        ".id" => "[uuid]",
    });
}

#[test]
fn test_tool_call_block_snapshot() {
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
fn test_tool_result_message_snapshot() {
    let msg = Message {
        id: MessageId::new(),
        role: Role::User,
        content: MessageContent::ToolResult {
            id: ToolCallId::new(),
            result: Ok("file1.txt\nfile2.txt".to_string()),
            attachments: Vec::new(),
        },
    };
    assert_json_snapshot!("tool_result_message", msg, {
        ".id" => "[uuid]",
        ".content.id" => "[uuid]",
    });
}

#[test]
fn test_tool_result_error_snapshot() {
    let msg = Message {
        id: MessageId::new(),
        role: Role::User,
        content: MessageContent::ToolResult {
            id: ToolCallId::new(),
            result: Err("command not found".to_string()),
            attachments: Vec::new(),
        },
    };
    assert_json_snapshot!("tool_result_error", msg, {
        ".id" => "[uuid]",
        ".content.id" => "[uuid]",
    });
}

#[test]
fn test_image_content_snapshot() {
    let content = Content::Image {
        media_type: "image/png".to_string(),
        data: vec![0x89, 0x50, 0x4E, 0x47],
    };
    assert_json_snapshot!("image_content", content);
}

#[test]
fn test_text_content_snapshot() {
    let content = Content::Text("Hello".to_string());
    assert_json_snapshot!("text_content", content);
}

#[test]
fn test_event_text_delta_snapshot() {
    let event = Event::TextDelta("Hello".to_string());
    assert_json_snapshot!("event_text_delta", event);
}

#[test]
fn test_event_tool_call_start_snapshot() {
    let event = Event::ToolCallStart {
        id: ToolCallId::new(),
        name: "read".to_string(),
        arguments: serde_json::json!({"path": "README.md"}),
    };
    assert_json_snapshot!("event_tool_call_start", event, {
        ".id" => "[uuid]",
    });
}

#[test]
fn test_event_usage_snapshot() {
    let event = Event::Usage {
        input_tokens: 100,
        output_tokens: 50,
        generation_ms: 1_250,
        estimated: false,
    };
    assert_json_snapshot!("event_usage", event);
}

#[test]
fn test_event_context_compacted_snapshot() {
    let event = Event::ContextCompacted {
        before_tokens: 180_000,
        after_tokens: 12_000,
        dropped_messages: 42,
        automatic: false,
    };
    assert_json_snapshot!("event_context_compacted", event);
}

#[test]
fn test_model_id_display() {
    let model = ModelId::new("claude-sonnet-4-20250514");
    assert_eq!(model.to_string(), "claude-sonnet-4-20250514");
}

#[test]
fn test_role_display() {
    assert_eq!(Role::User.to_string(), "User");
    assert_eq!(Role::Assistant.to_string(), "Assistant");
    assert_eq!(Role::System.to_string(), "System");
}
