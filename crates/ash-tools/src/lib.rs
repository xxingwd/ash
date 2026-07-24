mod bash;
mod edit;
mod glob;
mod grep;
mod path;
mod read;
mod truncate;
mod webfetch;
mod write;

use ash_core::{Tool, ToolError};
use std::sync::Arc;

pub fn tools(enabled: Option<&[String]>) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
    let tools = all_tools();
    let Some(names) = enabled else {
        return Ok(tools);
    };
    let unknown = names
        .iter()
        .filter(|name| !tools.iter().any(|tool| tool.name() == name.as_str()))
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        let available = tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ToolError::Execution(format!(
            "unknown tool(s): {}; available tools: {available}",
            unknown
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(tools
        .into_iter()
        .filter(|tool| names.iter().any(|name| name == tool.name()))
        .collect())
}

fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        read::tool(),
        glob::tool(),
        grep::tool(),
        bash::tool(),
        edit::tool(),
        write::tool(),
        webfetch::tool(),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn exposes_the_supported_tools_by_default() {
        let names = tools(None)
            .unwrap()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            ["read", "glob", "grep", "bash", "edit", "write", "webfetch"]
        );
    }

    #[test]
    fn selects_explicit_optional_tools_without_a_second_filtering_step() {
        let enabled = vec!["read".to_string(), "webfetch".to_string()];
        let names = tools(Some(&enabled))
            .unwrap()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, ["read", "webfetch"]);
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = tools(Some(&["read".into(), "reed".into()])).err().unwrap();

        assert!(error.to_string().contains("unknown tool(s): reed"));
        assert!(error.to_string().contains("available tools: read, glob"));
    }

    #[test]
    fn builtin_schemas_expose_only_the_supported_arguments() {
        let definitions = tools(None)
            .unwrap()
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool.definition()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            property_names(&definitions["read"]),
            ["limit", "offset", "path"]
        );
        assert_eq!(property_names(&definitions["glob"]), ["path", "pattern"]);
        assert_eq!(
            property_names(&definitions["grep"]),
            ["include", "path", "pattern"]
        );
        assert_eq!(property_names(&definitions["bash"]), ["command", "timeout"]);
        assert_eq!(property_names(&definitions["edit"]), ["edits", "path"]);
        assert_eq!(property_names(&definitions["write"]), ["content", "path"]);
        assert_eq!(property_names(&definitions["webfetch"]), ["timeout", "url"]);
        assert_eq!(required_names(&definitions["read"]), ["path"]);
        assert_eq!(required_names(&definitions["glob"]), ["pattern"]);
        assert_eq!(required_names(&definitions["grep"]), ["pattern"]);
        assert_eq!(required_names(&definitions["bash"]), ["command"]);
        assert_eq!(required_names(&definitions["edit"]), ["edits", "path"]);
        assert_eq!(required_names(&definitions["write"]), ["content", "path"]);
        assert_eq!(required_names(&definitions["webfetch"]), ["url"]);

        let edit_schema = &definitions["edit"].parameters_schema;
        let edit_items = &edit_schema["properties"]["edits"]["items"];
        let replacement_name = edit_items["$ref"]
            .as_str()
            .and_then(|reference| reference.rsplit('/').next())
            .unwrap();
        assert_eq!(
            edit_schema["definitions"][replacement_name]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["newText", "oldText"]
        );
    }

    fn property_names(definition: &ash_core::ToolDefinition) -> Vec<&str> {
        definition.parameters_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn required_names(definition: &ash_core::ToolDefinition) -> Vec<&str> {
        definition.parameters_schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .collect()
    }
}
