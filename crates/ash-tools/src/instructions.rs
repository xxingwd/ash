use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::{
    RepositoryInstruction, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolTimeout,
};

pub(crate) fn wrap(tool: Arc<dyn Tool>, cwd: PathBuf) -> Arc<dyn Tool> {
    if matches!(tool.name(), "read" | "write" | "edit" | "glob" | "grep") {
        Arc::new(ScopedTool { tool, cwd })
    } else {
        tool
    }
}

struct ScopedTool {
    tool: Arc<dyn Tool>,
    cwd: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ScopedTool {
    fn name(&self) -> &str {
        self.tool.name()
    }
    fn description(&self) -> &str {
        self.tool.description()
    }
    fn instructions(&self) -> Option<&str> {
        self.tool.instructions()
    }
    fn timeout(&self) -> ToolTimeout {
        self.tool.timeout()
    }
    fn definition(&self) -> ToolDefinition {
        self.tool.definition()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.parameters_schema()
    }

    async fn prepare(
        &self,
        context: ToolContext,
        args: &serde_json::Value,
        observed: &[RepositoryInstruction],
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let cwd = self.cwd.clone();
        let requested = cwd.join(path);
        let observed = observed.to_vec();
        context
            .run(tokio::task::spawn_blocking(move || {
                discover(&cwd, &requested, &observed)
            }))
            .await?
            .map_err(|error| ToolError::Execution(error.to_string()))?
    }

    async fn execute(
        &self,
        context: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.tool.execute(context, args).await
    }
}

fn discover(
    cwd: &Path,
    requested: &Path,
    observed: &[RepositoryInstruction],
) -> Result<Option<ToolOutput>, ToolError> {
    let mut existing = requested;
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| ToolError::Execution("cannot resolve instruction scope".into()))?;
    }
    let resolved = std::fs::canonicalize(existing).map_err(io_error)?;
    let directory = if resolved.is_dir() {
        resolved.as_path()
    } else {
        resolved.parent().unwrap_or(&resolved)
    };
    let root = directory
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or_else(|| {
            if directory.starts_with(cwd) {
                cwd
            } else {
                directory
            }
        });
    let mut directories = directory
        .ancestors()
        .take_while(|ancestor| *ancestor != root)
        .collect::<Vec<_>>();
    directories.push(root);
    directories.reverse();
    let mut rules = Vec::new();
    for directory in directories {
        let path = directory.join("AGENTS.md");
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(error)),
        };
        let mut bytes = Vec::new();
        file.take(65_537)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        let truncated = bytes.len() > 65_536;
        bytes.truncate(65_536);
        if let Err(error) = std::str::from_utf8(&bytes) {
            if truncated && error.error_len().is_none() {
                bytes.truncate(error.valid_up_to());
            } else {
                return Err(ToolError::Execution(format!("{}: {error}", path.display())));
            }
        }
        let text = String::from_utf8(bytes)
            .map_err(|error| ToolError::Execution(format!("{}: {error}", path.display())))?;
        let instruction = RepositoryInstruction {
            path,
            scope: directory.to_path_buf(),
            content: format!(
                "{}{}",
                text.trim(),
                if truncated {
                    "\n[truncated at 64 KiB; read the file for remaining instructions]"
                } else {
                    ""
                }
            ),
        };
        if !observed.contains(&instruction) {
            rules.push(instruction);
        }
    }
    if rules.is_empty() {
        return Ok(None);
    }
    Ok(Some(ToolOutput {
        text: format!("Applicable repository instructions follow. This call did not perform the requested file operation. Review them, then repeat the call.\n\n{}", rules.iter().map(RepositoryInstruction::render).collect::<Vec<_>>().join("\n\n")),
        observed_instructions: rules,
        ..ToolOutput::default()
    }))
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("cannot read repository instructions: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ToolContext {
        ToolContext {
            identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
            cancellation: ash_core::CancellationToken::new(),
            deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(30)),
        }
    }

    #[tokio::test]
    async fn new_scoped_rules_defer_writes_and_same_batch_cannot_bypass_them() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        std::fs::create_dir(directory.path().join("sibling")).unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "root rule").unwrap();
        std::fs::write(directory.path().join("nested/AGENTS.md"), "nested rule").unwrap();
        std::fs::write(directory.path().join("sibling/AGENTS.md"), "unrelated rule").unwrap();
        let tool = crate::tools(directory.path(), None)
            .unwrap()
            .into_iter()
            .find(|tool| tool.name() == "write")
            .unwrap();
        let args = serde_json::json!({"path":"nested/output.txt","content":"hello"});
        let (first, second) = tokio::join!(
            tool.prepare(context(), &args, &[]),
            tool.prepare(context(), &args, &[])
        );
        let first = first.unwrap().unwrap();
        assert!(second.unwrap().is_some());
        assert!(!directory.path().join("nested/output.txt").exists());
        assert!(first.text.contains("root rule"));
        assert!(first.text.contains("nested rule"));
        assert!(!first.text.contains("unrelated rule"));
        assert!(tool
            .prepare(context(), &args, &first.observed_instructions)
            .await
            .unwrap()
            .is_none());
        tool.execute(context(), args.clone()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("nested/output.txt")).unwrap(),
            "hello"
        );
        std::fs::write(directory.path().join("nested/AGENTS.md"), "updated rule").unwrap();
        assert!(tool
            .prepare(context(), &args, &first.observed_instructions)
            .await
            .unwrap()
            .unwrap()
            .text
            .contains("updated rule"));
    }

    #[test]
    fn invalid_encoding_is_not_treated_as_absent_rules() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), [255, 255]).unwrap();
        assert!(discover(directory.path(), directory.path(), &[]).is_err());
    }
}
