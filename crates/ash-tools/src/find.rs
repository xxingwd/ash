use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Glob pattern to match files (e.g. "**/*.rs")
    pattern: String,
    /// Directory to search in (defaults to working directory)
    path: Option<String>,
    /// Max results
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "find",
        "Find files matching a glob pattern",
        |ctx, args: FindArgs| async move {
            let root =
                crate::path::existing(&ctx.working_dir, args.path.as_deref().unwrap_or("."))?;

            let glob = globset::Glob::new(&args.pattern)
                .map_err(|e| ToolError::Execution(format!("invalid glob: {e}")))?
                .compile_matcher();

            let limit = args.limit.unwrap_or(100);
            let mut results = Vec::new();

            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if results.len() >= limit {
                    break;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&ctx.working_dir).unwrap_or(path);
                let rel_str = rel.to_string_lossy();
                if glob.is_match(rel_str.as_ref()) {
                    results.push(rel_str.into_owned());
                }
            }

            if results.is_empty() {
                Ok("no files found".into())
            } else {
                Ok(results.join("\n"))
            }
        },
    )
}
