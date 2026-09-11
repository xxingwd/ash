use ash_core::RepositoryInstruction;
use std::path::Path;

use chrono::Local;

pub const BASE_INSTRUCTIONS: &str = include_str!("../prompt.md");
const MAX_AGENTS_INSTRUCTIONS_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PromptContext {
    environment: String,
    #[serde(skip)]
    repository: Vec<RepositoryInstruction>,
}

impl PromptContext {
    pub fn load(working_dir: &Path) -> Result<Self, ash_core::AshError> {
        let working_dir = std::fs::canonicalize(working_dir).map_err(|error| {
            ash_core::AshError::Config(format!(
                "cannot resolve working directory {}: {error}",
                working_dir.display()
            ))
        })?;
        Ok(Self {
            environment: environment_context(&working_dir),
            repository: load_agents_instructions(&working_dir)?,
        })
    }

    pub(crate) fn environment(&self) -> &str {
        &self.environment
    }

    pub(crate) fn take_repository(&mut self) -> Vec<RepositoryInstruction> {
        std::mem::take(&mut self.repository)
    }
}

fn environment_context(working_dir: &Path) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    let now = Local::now();
    format!(
        "<environment_context>\n  <cwd>{}</cwd>\n  <shell>{}</shell>\n  \
         <current_date>{}</current_date>\n  <timezone>{}</timezone>\n  <os>{}</os>\n  \
         <arch>{}</arch>\n</environment_context>",
        escape_xml(&working_dir.to_string_lossy()),
        escape_xml(&shell),
        now.format("%Y-%m-%d"),
        now.format("%:z"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn load_agents_instructions(
    working_dir: &Path,
) -> Result<Vec<RepositoryInstruction>, ash_core::AshError> {
    let mut sections: Vec<RepositoryInstruction> = Vec::new();
    let mut remaining = MAX_AGENTS_INSTRUCTIONS_BYTES;
    for directory in crate::project::directories(working_dir) {
        let path = directory.join("AGENTS.md");
        if remaining == 0 {
            if let Some(last) = sections.last_mut() {
                last.content.push_str("\n[Additional repository instructions exceeded the initial 64 KiB budget. Inspect applicable AGENTS.md files before operating in a new scope.]");
            }
            break;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ash_core::AshError::Config(format!(
                    "cannot read {}: {error}",
                    path.display()
                )))
            }
        };
        let truncated = contents.trim().len() > remaining;
        let contents = truncate_utf8(contents.trim(), remaining);
        remaining = remaining.saturating_sub(contents.len());
        sections.push(RepositoryInstruction {
            path,
            scope: directory,
            content: format!(
                "{}{}",
                contents,
                if truncated {
                    "\n[truncated at the initial 64 KiB instruction budget]"
                } else {
                    ""
                }
            ),
        });
    }
    Ok(sections)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Skill;

    fn skill(path: &Path, name: &str, instructions: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            tools: None,
            model: None,
            instructions: instructions.to_string(),
            source_path: path.to_path_buf(),
        }
    }

    #[test]
    fn builds_environment_agents_and_skills_context() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "outside rules").unwrap();
        std::fs::write(project.join("AGENTS.md"), "project rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();
        std::fs::write(nested.join("AGENTS.override.md"), "ignored override").unwrap();
        let skill_path = project.join(".agents/skills/review/SKILL.md");
        let skill = skill(&skill_path, "review", "review carefully");

        let agent = crate::Agent::new(ash_core::ModelId::new("test"), Vec::new())
            .with_system_prompt(BASE_INSTRUCTIONS)
            .with_prompt_context(PromptContext::load(&nested).unwrap());
        let prompt = crate::install_skills(agent, vec![skill.clone()], Some(&skill))
            .unwrap()
            .system_prompt()
            .unwrap();

        assert!(prompt.contains("<environment_context>"));
        assert!(prompt.contains("project rules"));
        assert!(prompt.contains("nested rules"));
        assert!(!prompt.contains("outside rules"));
        assert!(!prompt.contains("ignored override"));
        assert!(prompt.find("project rules") < prompt.find("nested rules"));
        assert!(!prompt.contains("Use the `skill` tool to load a skill"));
        assert!(prompt.contains("`review`: review description"));
        assert!(!prompt.contains(".agents/skills/review/SKILL.md"));
        assert!(prompt.contains("## Active skill: review"));
        assert!(prompt.contains("review carefully"));
    }

    #[test]
    fn truncates_agents_instructions_on_utf8_boundaries() {
        assert_eq!(truncate_utf8("你好吗", 4), "你");
    }

    #[test]
    fn composes_explicit_role_tools_and_scoped_repository_in_order() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();
        let tool = ash_core::define_tool("example", "example", |_, ()| async { Ok("ok") }).unwrap();
        let agent = crate::Agent::new(ash_core::ModelId::new("model"), Vec::new())
            .with_system_prompt("explicit rules")
            .with_profile(
                crate::Profile::parse("test", "---\ndescription: test\n---\nrole rules").unwrap(),
            )
            .unwrap()
            .with_prompt_context(PromptContext::load(&nested).unwrap())
            .installing_tools([ash_core::with_tool_instructions(tool, "tool rules")])
            .unwrap();
        let prompt = agent.system_prompt().unwrap();
        let positions = [
            "explicit rules",
            "role rules",
            "<environment_context>",
            "tool rules",
            "root rules",
            "nested rules",
        ]
        .map(|section| prompt.find(section).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(prompt.contains(&nested.to_string_lossy().to_string()));
        assert_eq!(agent.clone().system_prompt(), Some(prompt));
    }
}
