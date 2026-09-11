use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::{define_tool, with_tool_instructions, ModelId, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::warn;

const SKILL_FILE_LIMIT: usize = 10;
pub const SKILL_TOOL_NAME: &str = "skill";
const SKILL_INSTRUCTIONS: &str = include_str!("../skill.md");

#[derive(Deserialize, JsonSchema)]
struct SkillArgs {
    /// Skill name from the available skills list
    name: String,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    tools: Option<Vec<String>>,
    model: Option<ModelId>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<ModelId>,
    pub(crate) instructions: String,
    pub(crate) source_path: PathBuf,
}

impl Skill {
    /// Discover skills from the working directory and the user skill
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns `AshError` when a skill directory cannot be read.
    pub fn discover(working_dir: &Path) -> Result<Vec<Self>, ash_core::AshError> {
        let user_skills = directories::BaseDirs::new()
            .map(|directories| directories.home_dir().join(".agents/skills"));
        Self::discover_from(working_dir, user_skills.as_deref())
    }

    fn discover_from(
        working_dir: &Path,
        user_skills: Option<&Path>,
    ) -> Result<Vec<Self>, ash_core::AshError> {
        let mut skills = BTreeMap::new();
        let mut load = |directory: &Path| -> Result<(), ash_core::AshError> {
            for skill in Self::load_from_dir(directory)? {
                skills.insert(skill.name.clone(), skill);
            }
            Ok(())
        };

        if let Some(directory) = user_skills {
            load(directory)?;
        }
        for directory in crate::project::directories(working_dir) {
            load(&directory.join(".agents/skills"))?;
        }
        Ok(skills.into_values().collect())
    }

    fn load_from_dir(dir: &Path) -> Result<Vec<Self>, ash_core::AshError> {
        let mut skills = Vec::new();

        if !dir.exists() {
            return Ok(skills);
        }

        let mut paths = Vec::new();
        for entry in std::fs::read_dir(dir)
            .map_err(|e| ash_core::AshError::Config(format!("cannot read skills dir: {e}")))?
        {
            let entry = entry.map_err(|e| ash_core::AshError::Config(e.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                let skill_path = path.join("SKILL.md");
                if skill_path.is_file() {
                    paths.push(skill_path);
                }
            }
        }
        paths.sort();

        for path in paths {
            match Self::load_file(&path) {
                Ok(skill) => skills.push(skill),
                Err(e) => {
                    warn!("failed to load skill {}: {e}", path.display());
                }
            }
        }

        Ok(skills)
    }

    fn load_file(path: &Path) -> Result<Self, ash_core::AshError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            ash_core::AshError::Config(format!("cannot read {}: {e}", path.display()))
        })?;

        let path = std::fs::canonicalize(path).map_err(|error| {
            ash_core::AshError::Config(format!("cannot resolve {}: {error}", path.display()))
        })?;
        Self::from_markdown(&path, &content)
    }

    pub fn from_markdown(path: &Path, content: &str) -> Result<Self, ash_core::AshError> {
        let (frontmatter, body) = parse_frontmatter(content)?;
        let metadata: SkillFrontmatter = serde_yaml::from_str(&frontmatter)
            .map_err(|e| ash_core::AshError::Config(format!("invalid skill frontmatter: {e}")))?;
        Ok(Self {
            name: metadata.name,
            description: metadata.description,
            tools: metadata.tools,
            model: metadata.model,
            instructions: body,
            source_path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn apply_overrides(&self, agent: crate::Agent) -> crate::Agent {
        match &self.model {
            Some(model) => agent.with_model(model.clone()),
            None => agent,
        }
    }

    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

/// Build the skill-loading tool.
///
/// # Errors
///
/// Returns `ToolError` when the tool cannot be defined.
pub fn tool(skills: Vec<Skill>) -> Result<Arc<dyn Tool>, ToolError> {
    tool_with_active(skills, None)
}

pub fn install(
    mut agent: crate::Agent,
    mut skills: Vec<Skill>,
    active_skill: Option<&Skill>,
) -> Result<crate::Agent, ToolError> {
    if skills.is_empty() && active_skill.is_none() {
        return Ok(agent);
    }
    if let Some(active) = active_skill {
        agent.apply_output(&ToolOutput {
            installed_tools: active.tools.clone().unwrap_or_default(),
            ..ToolOutput::default()
        })?;
        if !skills.iter().any(|skill| skill.name == active.name) {
            skills.push(active.clone());
        }
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    agent.installing_tools([tool_with_active(skills, active_skill)?])
}

fn tool_with_active(
    skills: Vec<Skill>,
    active_skill: Option<&Skill>,
) -> Result<Arc<dyn Tool>, ToolError> {
    let instructions = format!(
        "{}\n\n{}",
        SKILL_INSTRUCTIONS.trim(),
        skills_context(&skills, active_skill).unwrap_or_default()
    );
    let active_name = active_skill.map(|skill| skill.name.clone());
    let skills = Arc::new(skills);
    let tool = define_tool(
        SKILL_TOOL_NAME,
        "Load a specialized skill when its description matches the task. The name must match one of the skills listed in the system prompt.",
        move |_ctx, args: SkillArgs| {
            let skills = Arc::clone(&skills);
            let already_active = active_name.as_deref() == Some(args.name.as_str());
            async move {
                // Skill discovery and loading are blocking filesystem walks;
                // run them off the async worker threads.
                tokio::task::spawn_blocking(move || {
                    let text = load_skill(&skills, &args.name, already_active)?;
                    let installed_tools = skills.iter().find(|skill| skill.name == args.name)
                        .and_then(|skill| skill.tools.clone()).unwrap_or_default();
                    Ok::<_, ToolError>(ToolOutput { text, installed_tools, ..ToolOutput::default() })
                })
                    .await
                    .map_err(|error| {
                        ToolError::Execution(format!("skill loader task failed: {error}"))
                    })?
            }
        },
    )?;
    Ok(with_tool_instructions(tool, instructions))
}

pub(crate) fn skills_context(skills: &[Skill], active_skill: Option<&Skill>) -> Option<String> {
    if skills.is_empty() && active_skill.is_none() {
        return None;
    }
    let mut output = String::from("<skills>\n## Available skills\n");
    if skills.is_empty() {
        output.push_str("- None discovered.\n");
    } else {
        for skill in skills {
            let _ = writeln!(output, "- `{}`: {}", skill.name, skill.description);
        }
    }
    if let Some(skill) = active_skill {
        let _ = writeln!(
            output,
            "\n## Active skill: {}\n\n{}",
            skill.name,
            skill.instructions()
        );
    }
    output.push_str("</skills>");
    Some(output)
}

fn load_skill(skills: &[Skill], name: &str, already_active: bool) -> Result<String, ToolError> {
    let skill = skills
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| {
            let available = skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if available.is_empty() {
                String::new()
            } else {
                format!(" Available skills: {available}")
            };
            ToolError::Execution(format!("skill not found: {name}.{suffix}"))
        })?;
    let directory = skill.source_path.parent().unwrap_or_else(|| Path::new("."));
    let files = skill_files(skill)?;
    let mut output = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        if already_active {
            "This skill's instructions are already present in the system context.".into()
        } else {
            skill.instructions().trim().to_string()
        },
        String::new(),
        format!("Base directory for this skill: {}", directory.display()),
        "Relative paths in this skill are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    output.extend(
        files
            .iter()
            .map(|file| format!("<file>{}</file>", file.display())),
    );
    output.push("</skill_files>".into());
    output.push("</skill_content>".into());
    Ok(output.join("\n"))
}

fn skill_files(skill: &Skill) -> Result<Vec<PathBuf>, ToolError> {
    if skill.source_path.file_name() != Some(OsStr::new("SKILL.md")) {
        return Ok(Vec::new());
    }
    let root = skill.source_path.parent().unwrap_or_else(|| Path::new("."));
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            ToolError::Execution(format!("cannot inspect {}: {error}", path.display()))
        })?;
        if metadata.file_type().is_symlink() || path == skill.source_path {
            continue;
        }
        if metadata.is_file() {
            files.push(path);
            if files.len() == SKILL_FILE_LIMIT {
                return Ok(files);
            }
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(&path).map_err(|error| {
            ToolError::Execution(format!(
                "cannot list skill directory {}: {error}",
                path.display()
            ))
        })?;
        let mut paths = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                ToolError::Execution(format!(
                    "cannot list skill directory {}: {error}",
                    path.display()
                ))
            })?;
        paths.sort_by(|left, right| right.cmp(left));
        pending.extend(paths);
    }
    Ok(files)
}

fn parse_frontmatter(content: &str) -> Result<(String, String), ash_core::AshError> {
    crate::frontmatter::parse(content)
        .map(|(metadata, body)| (metadata.to_string(), body.to_string()))
        .map_err(|error| {
            ash_core::AshError::Config(match error {
                "file must start with ---" => "skill file must start with ---".into(),
                error => error.into(),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn installed_skill_owns_context_without_repeating_active_body() {
        let skill = Skill {
            name: "example".into(),
            description: "example description".into(),
            tools: None,
            model: None,
            instructions: "specialized rules".into(),
            source_path: PathBuf::from("example.md"),
        };
        let base = crate::Agent::new(ModelId::new("model"), Vec::new());
        let idle = install(base.clone(), vec![skill.clone()], None).unwrap();
        assert!(idle
            .system_prompt()
            .unwrap()
            .contains("example description"));
        assert!(!idle.system_prompt().unwrap().contains("specialized rules"));
        let agent = install(base.clone(), vec![skill.clone()], Some(&skill)).unwrap();
        let prompt = agent.system_prompt().unwrap();
        assert_eq!(prompt.matches("specialized rules").count(), 1);
        let output = agent.tools()[0]
            .execute(
                ash_core::ToolContext {
                    identity: ash_core::SessionIdentity::root(ash_core::SessionId::new()),
                    cancellation: ash_core::CancellationToken::new(),
                    deadline: None,
                },
                serde_json::json!({"name": "example"}),
            )
            .await
            .unwrap();
        assert!(!output.text.contains("specialized rules"));
        assert!(output.text.contains("already present"));
        assert!(output.text.contains("Base directory"));
        assert!(agent
            .without_tools(&[SKILL_TOOL_NAME])
            .system_prompt()
            .is_none());
        assert!(base.system_prompt().is_none());
        assert!(install(base, Vec::new(), None).unwrap().tools().is_empty());
    }

    #[test]
    fn loads_only_standard_skill_directories_in_stable_order() {
        let root = tempfile::tempdir().unwrap();
        let skills_dir = root.path().join(".agents/skills");
        std::fs::create_dir_all(skills_dir.join("nested")).unwrap();
        std::fs::write(
            skills_dir.join("z-last.md"),
            "---\nname: z-last\ndescription: last\n---\nlast instructions",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("nested/SKILL.md"),
            "---\nname: nested\ndescription: nested skill\n---\nnested instructions",
        )
        .unwrap();

        let skills = Skill::load_from_dir(&skills_dir).unwrap();

        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["nested"]
        );
        assert_eq!(skills[0].instructions(), "nested instructions");
        assert!(skills[0].source_path.ends_with("nested/SKILL.md"));
    }

    #[test]
    fn discovers_user_and_project_skills_with_nearest_project_override() {
        let root = tempfile::tempdir().unwrap();
        let home_skills = root.path().join("home/.agents/skills");
        let project = root.path().join("project");
        let nested = project.join("src");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        write_skill(&home_skills, "review", "user review");
        write_skill(
            &project.join(".agents/skills"),
            "release",
            "project release",
        );
        write_skill(&nested.join(".agents/skills"), "review", "project review");

        let skills = Skill::discover_from(&nested, Some(&home_skills)).unwrap();

        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["release", "review"]
        );
        assert_eq!(skills[1].instructions(), "project review");
    }

    #[test]
    fn runtime_tool_loads_instructions_and_resource_files() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join(".agents/skills/review");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: review code\n---\nReview carefully.",
        )
        .unwrap();
        std::fs::write(skill_dir.join("references/checklist.md"), "checklist").unwrap();
        let skills = Skill::load_from_dir(&root.path().join(".agents/skills")).unwrap();

        let output = load_skill(&skills, "review", false).unwrap();

        assert!(output.contains("<skill_content name=\"review\">"));
        assert!(output.contains("Review carefully."));
        assert!(output.contains(&skill_dir.display().to_string()));
        assert!(output.contains(
            &skill_dir
                .join("references/checklist.md")
                .display()
                .to_string()
        ));
        assert!(!output.contains("SKILL.md</file>"));
    }

    #[test]
    fn runtime_tool_exposes_only_its_name_argument() {
        let definition = tool(Vec::new()).unwrap().definition();
        let properties = definition.parameters_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();

        assert_eq!(properties, ["name"]);
        assert_eq!(
            definition.parameters_schema["required"],
            serde_json::json!(["name"])
        );
    }

    #[test]
    fn runtime_tool_reports_available_skills_for_unknown_names() {
        let skill = Skill {
            name: "review".into(),
            description: "review code".into(),
            tools: None,
            model: None,
            instructions: "Review carefully.".into(),
            source_path: PathBuf::from(".agents/skills/review/SKILL.md"),
        };

        let error = load_skill(&[skill], "missing", false).unwrap_err();

        assert!(error.to_string().contains("Available skills: review"));
    }

    #[test]
    fn frontmatter_delimiters_must_occupy_their_own_line() {
        let content = "---\nname: review\ndescription: Keep --- intact\n---\nInstructions";

        let (frontmatter, body) = parse_frontmatter(content).unwrap();

        assert!(frontmatter.contains("Keep --- intact"));
        assert_eq!(body, "Instructions");
    }

    fn write_skill(root: &Path, name: &str, instructions: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n{instructions}"),
        )
        .unwrap();
    }
}
