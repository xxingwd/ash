use std::sync::Arc;

use ash_core::{define_tool, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regular expression pattern to search for
    pattern: String,
    /// File or directory to search in (defaults to working directory)
    path: Option<String>,
    /// File glob pattern to filter (e.g. "*.rs")
    glob: Option<String>,
    /// Max results to return
    limit: Option<usize>,
}

pub fn tool() -> Arc<dyn Tool> {
    define_tool(
        "grep",
        "Search file contents using regex",
        |ctx, args: GrepArgs| async move {
            let root =
                crate::path::existing(&ctx.working_dir, args.path.as_deref().unwrap_or("."))?;

            let re = regex::Regex::new(&args.pattern)
                .map_err(|e| ToolError::Execution(format!("invalid regex: {e}")))?;

            let glob_filter = args
                .glob
                .as_ref()
                .map(|g| {
                    globset::GlobBuilder::new(g)
                        .literal_separator(true)
                        .build()
                        .and_then(|g| globset::GlobSet::builder().add(g).build())
                })
                .transpose()
                .map_err(|e| ToolError::Execution(format!("invalid glob: {e}")))?;

            let limit = args.limit.unwrap_or(50);
            let mut results = Vec::new();
            let mut count = 0;

            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                if count >= limit {
                    break;
                }

                let path = entry.path();
                if let Some(ref globs) = glob_filter {
                    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                    if !globs.is_match(file_name.as_ref()) {
                        continue;
                    }
                }

                let content = match tokio::fs::read_to_string(path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                for (line_num, line) in content.lines().enumerate() {
                    if count >= limit {
                        break;
                    }
                    if re.is_match(line) {
                        let rel = path.strip_prefix(&ctx.working_dir).unwrap_or(path);
                        results.push(format!(
                            "{}:{}:{}",
                            rel.display(),
                            line_num + 1,
                            line.trim()
                        ));
                        count += 1;
                    }
                }
            }

            if results.is_empty() {
                Ok("no matches found".into())
            } else {
                Ok(results.join("\n"))
            }
        },
    )
}
