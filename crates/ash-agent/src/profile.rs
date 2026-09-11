use std::{collections::HashSet, sync::Arc};

use ash_core::{AshError, Tool};
use serde::{Deserialize, Serialize};

const BUILTINS: [(&str, &str); 3] = [
    ("default", include_str!("../agents/default.md")),
    ("explore", include_str!("../agents/explore.md")),
    ("review", include_str!("../agents/review.md")),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    name: String,
    description: String,
    instructions: String,
    tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    description: String,
    tools: Option<String>,
}

impl Profile {
    pub fn parse(name: &str, markdown: &str) -> Result<Self, AshError> {
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(AshError::Config(format!("invalid profile name: {name}")));
        }
        let (frontmatter, instructions) = crate::frontmatter::parse(markdown)
            .map_err(|error| AshError::Config(format!("profile {name}: {error}")))?;
        let metadata: Metadata = serde_yaml::from_str(frontmatter)
            .map_err(|error| AshError::Config(format!("invalid profile {name}: {error}")))?;
        if metadata.description.trim().is_empty() || instructions.is_empty() {
            return Err(AshError::Config(format!(
                "profile {name} requires a description and instructions"
            )));
        }
        let tools = metadata.tools.as_deref().map(parse_tools).transpose()?;
        Ok(Self {
            name: name.to_string(),
            description: metadata.description.trim().to_string(),
            instructions: instructions.to_string(),
            tools,
        })
    }

    pub fn builtin(name: &str) -> Result<Self, AshError> {
        let (_, markdown) = BUILTINS
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .ok_or_else(|| {
                AshError::Config(format!(
                    "unknown profile: {name}; available: {}",
                    BUILTINS
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        Self::parse(name, markdown)
    }

    pub fn builtins() -> Result<Vec<Self>, AshError> {
        BUILTINS
            .iter()
            .map(|(name, markdown)| Self::parse(name, markdown))
            .collect()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn tools(&self) -> Option<&[String]> {
        self.tools.as_deref()
    }

    pub(crate) fn select_tools(
        &self,
        available: &[Arc<dyn Tool>],
    ) -> Result<Vec<Arc<dyn Tool>>, AshError> {
        let Some(names) = &self.tools else {
            return Ok(available.to_vec());
        };
        let unknown = names
            .iter()
            .filter(|name| !available.iter().any(|tool| tool.name() == name.as_str()))
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(AshError::Config(format!(
                "profile {} references unavailable tools: {unknown:?}",
                self.name
            )));
        }
        Ok(available
            .iter()
            .filter(|tool| names.iter().any(|name| name == tool.name()))
            .cloned()
            .collect())
    }
}

fn parse_tools(value: &str) -> Result<Vec<String>, AshError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    value
        .split(',')
        .map(str::trim)
        .filter(|name| seen.insert(*name))
        .map(|name| {
            if name.is_empty() {
                Err(AshError::Config(
                    "profile tools contains an empty entry".into(),
                ))
            } else {
                Ok(name.to_string())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Agent;
    use ash_core::{define_tool, ModelId};

    #[test]
    fn loads_embedded_profiles_without_a_source_directory() {
        let profiles = Profile::builtins().unwrap();
        assert_eq!(
            profiles.iter().map(Profile::name).collect::<Vec<_>>(),
            ["default", "explore", "review"]
        );
        assert!(profiles
            .iter()
            .all(|profile| !profile.instructions().is_empty()));
        assert_eq!(profiles[0].tools(), None);
        assert_eq!(profiles[1].tools(), profiles[2].tools());
    }

    #[test]
    fn parses_tool_strings_without_confusing_empty_and_missing() {
        let parse = |tools: &str| {
            Profile::parse(
                "test",
                &format!("---\ndescription: test\n{tools}\n---\nbody"),
            )
            .unwrap()
        };
        assert_eq!(parse("").tools(), None);
        assert_eq!(parse("tools: ''").tools(), Some([].as_slice()));
        assert_eq!(
            parse("tools: 'read, bash, read'").tools(),
            Some(["read".into(), "bash".into()].as_slice())
        );
    }

    #[test]
    fn rejects_invalid_metadata_names_and_empty_instructions() {
        for markdown in [
            "---\ntools: read\n---\nbody",
            "---\ndescription: ''\n---\nbody",
            "---\ndescription: test\n---\n",
            "---\ndescription: test\nmodel: x\n---\nbody",
            "---\ndescription: test\ntools: [read]\n---\nbody",
            "---\ndescription: test\ntools: 'read,,bash'\n---\nbody",
        ] {
            assert!(Profile::parse("test", markdown).is_err(), "{markdown}");
        }
        assert!(Profile::parse("../review", "---\ndescription: test\n---\nbody").is_err());
        assert!(Profile::builtin("defualt").is_err());
        assert!(Profile::builtin("").is_err());
    }

    #[test]
    fn validates_tools_before_applying_the_profile() {
        let profile =
            Profile::parse("test", "---\ndescription: test\ntools: missing\n---\nbody").unwrap();
        let agent = Agent::new(
            ModelId::new("test"),
            vec![define_tool("read", "read", |_, ()| async { Ok("ok") }).unwrap()],
        );
        assert!(agent.with_profile(profile).is_err());
    }

    #[test]
    fn serialized_profile_retains_its_definition() {
        let profile = Profile::builtin("review").unwrap();
        let encoded = serde_json::to_string(&profile).unwrap();
        assert_eq!(serde_json::from_str::<Profile>(&encoded).unwrap(), profile);
    }
}
