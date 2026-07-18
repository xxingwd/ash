mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod path;
mod read;
mod truncate;
mod write;

use ash_core::Tool;
use std::sync::Arc;

pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    all_tools()
}

pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    tools(None)
}

pub fn tools(enabled: Option<&[String]>) -> Vec<Arc<dyn Tool>> {
    let default = ["read", "bash", "edit", "write"];
    all_tools()
        .into_iter()
        .filter(|tool| {
            enabled.map_or_else(
                || default.contains(&tool.name()),
                |enabled| enabled.iter().any(|name| name == tool.name()),
            )
        })
        .collect()
}

fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        read::tool(),
        bash::tool(),
        edit::tool(),
        write::tool(),
        grep::tool(),
        find::tool(),
        ls::tool(),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn exposes_only_the_seven_supported_builtin_tools() {
        let names = builtin_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            ["read", "bash", "edit", "write", "grep", "find", "ls"]
        );
    }

    #[test]
    fn enables_only_coding_tools_by_default() {
        let names = default_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, ["read", "bash", "edit", "write"]);
    }

    #[test]
    fn selects_explicit_optional_tools_without_a_second_filtering_step() {
        let enabled = vec!["read".to_string(), "grep".to_string(), "ls".to_string()];
        let names = tools(Some(&enabled))
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, ["read", "grep", "ls"]);
    }

    #[test]
    fn builtin_schemas_expose_only_the_supported_arguments() {
        let definitions = builtin_tools()
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool.definition()))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            property_names(&definitions["read"]),
            ["limit", "offset", "path"]
        );
        assert_eq!(property_names(&definitions["bash"]), ["command", "timeout"]);
        assert_eq!(property_names(&definitions["edit"]), ["edits", "path"]);
        assert_eq!(property_names(&definitions["write"]), ["content", "path"]);
        assert_eq!(
            property_names(&definitions["grep"]),
            [
                "context",
                "glob",
                "ignoreCase",
                "limit",
                "literal",
                "path",
                "pattern",
            ]
        );
        assert_eq!(
            property_names(&definitions["find"]),
            ["limit", "path", "pattern"]
        );
        assert_eq!(property_names(&definitions["ls"]), ["limit", "path"]);
        assert_eq!(required_names(&definitions["read"]), ["path"]);
        assert_eq!(required_names(&definitions["bash"]), ["command"]);
        assert_eq!(required_names(&definitions["edit"]), ["edits", "path"]);
        assert_eq!(required_names(&definitions["write"]), ["content", "path"]);
        assert_eq!(required_names(&definitions["grep"]), ["pattern"]);
        assert_eq!(required_names(&definitions["find"]), ["pattern"]);
        assert!(required_names(&definitions["ls"]).is_empty());

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
