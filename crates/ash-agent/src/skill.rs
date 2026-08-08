use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use ash_core::{define_tool, ModelId, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

const SKILL_FILE_LIMIT: usize = 10;
pub(crate) const SKILL_TOOL_NAME: &str = "skill";

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
    pub fn discover(working_dir: &Path) -> Result<Vec<Skill>, ash_core::AshError> {
        let user_skills = directories::BaseDirs::new()
            .map(|directories| directories.home_dir().join(".agents/skills"));
        Self::discover_from(working_dir, user_skills.as_deref())
    }

    fn discover_from(
        working_dir: &Path,
        user_skills: Option<&Path>,
    ) -> Result<Vec<Skill>, ash_core::AshError> {
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

    fn load_from_dir(dir: &Path) -> Result<Vec<Skill>, ash_core::AshError> {
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
                    tracing::warn!("failed to load skill {}: {e}", path.display());
                }
            }
        }

        Ok(skills)
    }

    fn load_file(path: &Path) -> Result<Skill, ash_core::AshError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            ash_core::AshError::Config(format!("cannot read {}: {e}", path.display()))
        })?;

        let (frontmatter, body) = parse_frontmatter(&content)?;
        let metadata: SkillFrontmatter = serde_yaml::from_str(&frontmatter)
            .map_err(|e| ash_core::AshError::Config(format!("invalid skill frontmatter: {e}")))?;
        Ok(Skill {
            name: metadata.name,
            description: metadata.description,
            tools: metadata.tools,
            model: metadata.model,
            instructions: body,
            source_path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        })
    }

    pub fn apply_overrides(&self, agent: &mut crate::Agent) {
        if let Some(model) = &self.model {
            agent.model = model.clone();
        }
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

pub fn tool(skills: Vec<Skill>) -> Result<Arc<dyn Tool>, ToolError> {
    let skills = Arc::new(skills);
    define_tool(
        SKILL_TOOL_NAME,
        "Load a specialized skill when its description matches the task. The name must match one of the skills listed in the system prompt.",
        move |_ctx, args: SkillArgs| {
            let skills = Arc::clone(&skills);
            async move { load_skill(&skills, &args.name) }
        },
    )
}

fn load_skill(skills: &[Skill], name: &str) -> Result<String, ToolError> {
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
        skill.instructions().trim().to_string(),
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
    let content = content.trim_start();
    let mut lines = content.split_inclusive('\n');
    let first_line = lines.next().unwrap_or_default();
    if first_line.trim_end_matches(['\r', '\n']) != "---" {
        return Err(ash_core::AshError::Config(
            "skill file must start with ---".into(),
        ));
    }

    let frontmatter_start = first_line.len();
    let mut offset = frontmatter_start;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((
                content[frontmatter_start..offset].trim().to_string(),
                content[offset + line.len()..].trim().to_string(),
            ));
        }
        offset += line.len();
    }
    Err(ash_core::AshError::Config("missing closing ---".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let output = load_skill(&skills, "review").unwrap();

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

        let error = load_skill(&[skill], "missing").unwrap_err();

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
