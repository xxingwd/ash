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
    /// Returns `AshError::Config` when the segment is not a valid task name.
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

    /// Whether this path sits inside `prefix` on a segment boundary, so
    /// `/root/a/b` matches the prefix `/root/a` but not `/root/ab`.
    #[must_use]
    pub fn under(&self, prefix: &str) -> bool {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return true;
        }
        self.0 == prefix || self.0.starts_with(&format!("{prefix}/"))
    }
}

impl std::fmt::Display for AgentPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One validated path segment: lowercase letters, digits, or underscores.
fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.chars().count() <= MAX_SEGMENT_CHARS
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
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

    /// The identity of a child session spawned by `self` for `task_name`.
    ///
    /// # Errors
    ///
    /// Returns `AshError::Config` when the task name is not a valid path
    /// segment.
    pub fn child(&self, id: SessionId, task_name: &str) -> Result<Self, AshError> {
        Ok(Self {
            id,
            root_id: self.root_id,
            parent_id: Some(self.id),
            path: self.path.join(task_name)?,
        })
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Restore a path from persisted storage, rejecting anything that could not
/// have been produced by `AgentPath::join`.
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
        let nested = path.join("deep_dive").unwrap();
        assert_eq!(nested.as_str(), "/root/research/deep_dive");
        assert_eq!(
            nested.segments().collect::<Vec<_>>(),
            ["research", "deep_dive"]
        );
    }

    #[test]
    fn joining_rejects_invalid_segments() {
        for segment in [
            "",
            "/x",
            "Task",
            "a-b",
            "a b",
            &"a".repeat(MAX_SEGMENT_CHARS + 1),
        ] {
            assert!(AgentPath::root().join(segment).is_err(), "{segment:?}");
        }
    }

    #[test]
    fn prefix_matching_honors_segment_boundaries() {
        let path = AgentPath::root().join("ab").unwrap();
        assert!(path.under("/root"));
        assert!(path.under("/root/ab"));
        assert!(!path.under("/root/a"));
        assert!(path.under(""));
    }

    #[test]
    fn child_identity_derives_lineage_from_the_parent() {
        let root = SessionIdentity::root(SessionId::new());
        let child = root.child(SessionId::new(), "research").unwrap();
        assert_eq!(child.root_id, root.id);
        assert_eq!(child.parent_id, Some(root.id));
        assert_eq!(child.path.as_str(), "/root/research");
        assert!(child.path.under(root.path.as_str()));

        let grandchild = child.child(SessionId::new(), "scan").unwrap();
        assert_eq!(grandchild.root_id, root.id);
        assert_eq!(grandchild.parent_id, Some(child.id));
        assert_eq!(grandchild.path.as_str(), "/root/research/scan");
    }

    #[test]
    fn parsing_accepts_only_canonical_paths() {
        assert_eq!(parse_agent_path("/root").unwrap(), AgentPath::root());
        assert!(parse_agent_path("/root/research").is_ok());
        for value in [
            "",
            "root",
            "/",
            "/home/user",
            "/root/Task",
            "/root//x",
            "/root/a/b/",
        ] {
            assert!(parse_agent_path(value).is_err(), "{value:?}");
        }
    }
}
