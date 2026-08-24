use serde::{Deserialize, Serialize};

use crate::{AshError, SessionId};

const MAX_SEGMENT_CHARS: usize = 64;
const ROOT_PATH: &str = "/root";

/// A canonical path inside one collaboration tree, such as `/root/research`.
///
/// The root session is always `/root`; every child appends one validated
/// segment. Paths are agent-tree coordinates and never filesystem locations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentPath(String);

impl AgentPath {
    /// The path of the root session: `/root`.
    #[must_use]
    pub fn root() -> Self {
        Self(ROOT_PATH.to_string())
    }

    /// Append one segment, producing the path of a child session.
    ///
    /// # Errors
    ///
    /// Returns `AshError::Config` when the segment is not a valid agent name.
    pub fn join(&self, segment: &str) -> Result<Self, AshError> {
        if !is_valid_segment(segment) {
            return Err(AshError::Config(format!(
                "invalid agent path segment: {segment}"
            )));
        }
        Ok(Self(format!("{}/{segment}", self.0)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments after `/root`, in tree order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.trim_start_matches('/').split('/').skip(1)
    }
}

impl std::fmt::Display for AgentPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Whether `segment` is a stable agent identifier: lowercase ASCII letters,
/// digits, underscores, or hyphens, with a bounded persisted length.
#[must_use]
pub fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_CHARS
        && segment.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

/// The durable identity of one session inside its collaboration tree.
///
/// The root session satisfies `root_id == id`, `parent_id == None`, and
/// `path == /root`. A child derives everything from its parent plus one task
/// name, so lineage is always reconstructible from persisted metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub id: SessionId,
    pub root_id: SessionId,
    pub parent_id: Option<SessionId>,
    pub path: AgentPath,
}

impl SessionIdentity {
    /// The identity of a root session: its own tree rooted at `/root`.
    #[must_use]
    pub fn root(id: SessionId) -> Self {
        Self {
            id,
            root_id: id,
            parent_id: None,
            path: AgentPath::root(),
        }
    }

    /// The identity of a child session spawned by `self` for `agent_name`.
    ///
    /// # Errors
    ///
    /// Returns `AshError::Config` when the agent name is not a valid path
    /// segment.
    pub fn child(&self, id: SessionId, agent_name: &str) -> Result<Self, AshError> {
        Ok(Self {
            id,
            root_id: self.root_id,
            parent_id: Some(self.id),
            path: self.path.join(agent_name)?,
        })
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Restore a path from persisted storage, rejecting anything that could not
/// have been produced by `AgentPath::join`.
///
/// # Errors
///
/// Returns `AshError::Config` when the path is not a valid canonical agent path.
pub fn parse_agent_path(value: &str) -> Result<AgentPath, AshError> {
    let invalid = || AshError::Config(format!("invalid agent path in session header: {value}"));
    let body = value.strip_prefix('/').ok_or_else(invalid)?;
    if body.is_empty() {
        return Err(invalid());
    }
    let mut segments = body.split('/');
    if segments.next() != Some("root") {
        return Err(invalid());
    }
    if !segments.all(is_valid_segment) {
        return Err(invalid());
    }
    Ok(AgentPath(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path_is_constant() {
        assert_eq!(AgentPath::root().as_str(), "/root");
    }

    #[test]
    fn joining_builds_nested_paths() {
        let path = AgentPath::root().join("research").unwrap();
        assert_eq!(path.as_str(), "/root/research");
        let nested = path.join("deep-dive").unwrap();
        assert_eq!(nested.as_str(), "/root/research/deep-dive");
        assert_eq!(
            nested.segments().collect::<Vec<_>>(),
            ["research", "deep-dive"]
        );
    }

    #[test]
    fn joining_accepts_stable_slug_names_with_hyphens() {
        for segment in ["be-resource-product", "research_agent", "worker2"] {
            assert!(AgentPath::root().join(segment).is_ok(), "{segment:?}");
        }
    }

    #[test]
    fn joining_rejects_noncanonical_segments() {
        let long_name = "a".repeat(MAX_SEGMENT_CHARS + 1);
        for segment in [
            "",
            "/x",
            "a/b",
            "Resource Product",
            "资源梳理",
            long_name.as_str(),
        ] {
            assert!(AgentPath::root().join(segment).is_err(), "{segment:?}");
        }
    }

    #[test]
    fn child_identity_derives_lineage_from_the_parent() {
        let root = SessionIdentity::root(SessionId::new());
        let child = root.child(SessionId::new(), "research").unwrap();
        assert_eq!(child.root_id, root.id);
        assert_eq!(child.parent_id, Some(root.id));
        assert_eq!(child.path.as_str(), "/root/research");

        let grandchild = child.child(SessionId::new(), "scan").unwrap();
        assert_eq!(grandchild.root_id, root.id);
        assert_eq!(grandchild.parent_id, Some(child.id));
        assert_eq!(grandchild.path.as_str(), "/root/research/scan");
    }

    #[test]
    fn parsing_accepts_only_canonical_paths() {
        assert_eq!(parse_agent_path("/root").unwrap(), AgentPath::root());
        assert!(parse_agent_path("/root/research").is_ok());
        assert!(parse_agent_path("/root/resource-product").is_ok());
        for value in [
            "",
            "root",
            "/",
            "/home/user",
            "/root/Resource Product",
            "/root//x",
            "/root/a/b/",
            "/root/two\nlines",
        ] {
            assert!(parse_agent_path(value).is_err(), "{value:?}");
        }
    }
}
