use std::path::{Path, PathBuf};

use ash_core::ModelId;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<ModelId>,
    #[serde(skip)]
    pub(crate) source_path: PathBuf,
}

impl Skill {
    pub fn load_from_dir(dir: &Path) -> Result<Vec<Skill>, ash_core::AshError> {
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
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                paths.push(path);
            } else if path.is_dir() {
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
        let mut skill: Skill = toml::from_str(&frontmatter)
            .map_err(|e| ash_core::AshError::Config(format!("invalid skill frontmatter: {e}")))?;
        skill.system_prompt = body;
        skill.source_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        Ok(skill)
    }

    pub fn apply_overrides(&self, config: &mut crate::AgentConfig) {
        if let Some(model) = &self.model {
            config.model = model.clone();
        }
    }

    pub fn instructions(&self) -> &str {
        &self.system_prompt
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }
}

fn parse_frontmatter(content: &str) -> Result<(String, String), ash_core::AshError> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Err(ash_core::AshError::Config(
            "skill file must start with ---".into(),
        ));
    }

    let rest = &content[3..];
    let end = rest
        .find("---")
        .ok_or_else(|| ash_core::AshError::Config("missing closing ---".into()))?;

    let frontmatter = rest[..end].trim().to_string();
    let body = rest[end + 3..].trim().to_string();

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_flat_and_nested_skill_files_in_stable_order() {
        let root = tempfile::tempdir().unwrap();
        let skills_dir = root.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("nested")).unwrap();
        std::fs::write(
            skills_dir.join("z-last.md"),
            "---\nname = \"z-last\"\ndescription = \"last\"\n---\nlast instructions",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("nested/SKILL.md"),
            "---\nname = \"nested\"\ndescription = \"nested skill\"\n---\nnested instructions",
        )
        .unwrap();

        let skills = Skill::load_from_dir(&skills_dir).unwrap();

        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["nested", "z-last"]
        );
        assert_eq!(skills[0].instructions(), "nested instructions");
        assert!(skills[0].source_path().ends_with("nested/SKILL.md"));
    }
}
