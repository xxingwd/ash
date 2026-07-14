use std::path::{Path, PathBuf};

use chrono::Local;

use crate::Skill;

const BASE_INSTRUCTIONS: &str = include_str!("../prompt.md");
const MAX_AGENTS_INSTRUCTIONS_BYTES: usize = 64 * 1024;

pub fn build_system_prompt(
    working_dir: &Path,
    skills: &[Skill],
    active_skill: Option<&Skill>,
) -> Result<String, ash_core::AshError> {
    let working_dir = std::fs::canonicalize(working_dir).map_err(|error| {
        ash_core::AshError::Config(format!(
            "cannot resolve working directory {}: {error}",
            working_dir.display()
        ))
    })?;

    let mut sections = vec![BASE_INSTRUCTIONS.trim().to_string()];
    sections.push(environment_context(&working_dir));
    sections.extend(load_agents_instructions(&working_dir)?);
    if let Some(skills) = skills_context(&working_dir, skills, active_skill) {
        sections.push(skills);
    }
    Ok(sections.join("\n\n"))
}

fn environment_context(working_dir: &Path) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    let now = Local::now();
    format!(
        "<environment_context>\n  <cwd>{}</cwd>\n  <shell>{}</shell>\n  \
         <current_date>{}</current_date>\n  <timezone>{}</timezone>\n  <os>{}</os>\n  \
         <arch>{}</arch>\n</environment_context>",
        escape_xml(&working_dir.display().to_string()),
        escape_xml(&shell),
        now.format("%Y-%m-%d"),
        now.format("%:z"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn load_agents_instructions(working_dir: &Path) -> Result<Vec<String>, ash_core::AshError> {
    let mut directories = working_dir
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.reverse();

    let mut sections = Vec::new();
    let mut remaining = MAX_AGENTS_INSTRUCTIONS_BYTES;
    for directory in directories {
        let override_path = directory.join("AGENTS.override.md");
        let default_path = directory.join("AGENTS.md");
        let path = if override_path.is_file() {
            override_path
        } else if default_path.is_file() {
            default_path
        } else {
            continue;
        };
        if remaining == 0 {
            break;
        }

        let contents = std::fs::read_to_string(&path).map_err(|error| {
            ash_core::AshError::Config(format!("cannot read {}: {error}", path.display()))
        })?;
        let contents = truncate_utf8(contents.trim(), remaining);
        remaining = remaining.saturating_sub(contents.len());
        sections.push(format!(
            "# AGENTS.md instructions for {}\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
            directory.display(),
            contents
        ));
    }
    Ok(sections)
}

fn skills_context(
    working_dir: &Path,
    skills: &[Skill],
    active_skill: Option<&Skill>,
) -> Option<String> {
    if skills.is_empty() && active_skill.is_none() {
        return None;
    }

    let mut output = String::from("<skills>\n## Available skills\n");
    if skills.is_empty() {
        output.push_str("- None discovered.\n");
    } else {
        for skill in skills {
            let path = display_path(working_dir, skill.source_path());
            output.push_str(&format!(
                "- `{}`: {} (file: {})\n",
                skill.name,
                skill.description,
                path.display()
            ));
        }
    }

    if let Some(skill) = active_skill {
        output.push_str(&format!(
            "\n## Active skill: {}\n\n{}\n",
            skill.name,
            skill.instructions()
        ));
    }
    output.push_str("</skills>");
    Some(output)
}

fn display_path(working_dir: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(working_dir).unwrap_or(path).to_path_buf()
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

    fn skill(path: &Path, name: &str, instructions: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            system_prompt: instructions.to_string(),
            tools: None,
            model: None,
            source_path: path.to_path_buf(),
        }
    }

    #[test]
    fn builds_environment_agents_and_skills_context() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("AGENTS.md"), "project rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "ignored rules").unwrap();
        std::fs::write(nested.join("AGENTS.override.md"), "nested override").unwrap();
        let skill_path = project.join("skills/review.md");
        let skill = skill(&skill_path, "review", "review carefully");

        let prompt =
            build_system_prompt(&nested, std::slice::from_ref(&skill), Some(&skill)).unwrap();

        assert!(prompt.contains("<environment_context>"));
        assert!(prompt.contains("project rules"));
        assert!(prompt.contains("nested override"));
        assert!(!prompt.contains("ignored rules"));
        assert!(prompt.find("project rules") < prompt.find("nested override"));
        assert!(prompt.contains("`review`: review description"));
        assert!(prompt.contains("## Active skill: review"));
        assert!(prompt.contains("review carefully"));
    }

    #[test]
    fn truncates_agents_instructions_on_utf8_boundaries() {
        assert_eq!(truncate_utf8("你好吗", 4), "你");
    }
}
