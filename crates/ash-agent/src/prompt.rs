use std::path::Path;

use chrono::Local;

use crate::Skill;

const BASE_INSTRUCTIONS: &str = include_str!("../prompt.md");
const MAX_AGENTS_INSTRUCTIONS_BYTES: usize = 64 * 1024;

/// Build the full system prompt for a working directory: base instructions,
/// environment context, AGENTS.md instructions, and the skills list.
///
/// This performs synchronous filesystem reads (canonicalization, AGENTS.md
/// discovery). Callers in an async context must wrap it in
/// `tokio::task::spawn_blocking` so the blocking IO stays off the async
/// worker threads.
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
    if let Some(skills) = skills_context(skills, active_skill) {
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
    let mut sections = Vec::new();
    let mut remaining = MAX_AGENTS_INSTRUCTIONS_BYTES;
    for directory in crate::project::directories(working_dir) {
        let path = directory.join("AGENTS.md");
        if !path.is_file() {
            continue;
        }
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

fn skills_context(skills: &[Skill], active_skill: Option<&Skill>) -> Option<String> {
    if skills.is_empty() && active_skill.is_none() {
        return None;
    }

    let mut output = String::from(
        "<skills>\nSkills provide specialized instructions and workflows for specific tasks.\n\
         Use the `skill` tool to load a skill when a task matches its description.\n\n\
         ## Available skills\n",
    );
    if skills.is_empty() {
        output.push_str("- None discovered.\n");
    } else {
        for skill in skills {
            output.push_str(&format!("- `{}`: {}\n", skill.name, skill.description));
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

        let prompt =
            build_system_prompt(&nested, std::slice::from_ref(&skill), Some(&skill)).unwrap();

        assert!(prompt.contains("<environment_context>"));
        assert!(prompt.contains("project rules"));
        assert!(prompt.contains("nested rules"));
        assert!(!prompt.contains("outside rules"));
        assert!(!prompt.contains("ignored override"));
        assert!(prompt.find("project rules") < prompt.find("nested rules"));
        assert!(prompt.contains("Use the `skill` tool to load a skill"));
        assert!(prompt.contains("`review`: review description"));
        assert!(!prompt.contains(".agents/skills/review/SKILL.md"));
        assert!(prompt.contains("## Active skill: review"));
        assert!(prompt.contains("review carefully"));
    }

    #[test]
    fn truncates_agents_instructions_on_utf8_boundaries() {
        assert_eq!(truncate_utf8("你好吗", 4), "你");
    }
}
