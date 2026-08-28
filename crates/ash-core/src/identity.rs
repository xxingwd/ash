use serde::{Deserialize, Serialize};

use crate::SessionId;

/// The durable identity of one session inside its collaboration tree.
///
/// The root session satisfies `root_id == id` and `parent_id == None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SessionIdentity {
    id: SessionId,
    root_id: SessionId,
    parent_id: Option<SessionId>,
}

impl SessionIdentity {
    /// The identity of a root session.
    #[must_use]
    pub const fn root(id: SessionId) -> Self {
        Self {
            id,
            root_id: id,
            parent_id: None,
        }
    }

    /// Create the identity of a direct child session spawned by `self`.
    #[must_use]
    pub fn child(self) -> Self {
        Self {
            id: SessionId::new(),
            root_id: self.root_id,
            parent_id: Some(self.id),
        }
    }

    #[must_use]
    pub fn try_from_parts(
        id: SessionId,
        root_id: SessionId,
        parent_id: Option<SessionId>,
    ) -> Option<Self> {
        let valid_root = parent_id.is_none() && id == root_id;
        let valid_child = match parent_id {
            Some(parent_id) => id != root_id && parent_id != id,
            None => false,
        };
        if valid_root || valid_child {
            Some(Self {
                id,
                root_id,
                parent_id,
            })
        } else {
            None
        }
    }

    #[must_use]
    pub const fn id(self) -> SessionId {
        self.id
    }

    #[must_use]
    pub const fn root_id(self) -> SessionId {
        self.root_id
    }

    #[must_use]
    pub const fn parent_id(self) -> Option<SessionId> {
        self.parent_id
    }

    #[must_use]
    pub fn is_root(self) -> bool {
        self.parent_id.is_none()
    }
}

impl<'de> Deserialize<'de> for SessionIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Parts {
            id: SessionId,
            root_id: SessionId,
            parent_id: Option<SessionId>,
        }

        let parts = Parts::deserialize(deserializer)?;
        Self::try_from_parts(parts.id, parts.root_id, parts.parent_id)
            .ok_or_else(|| serde::de::Error::custom("invalid session identity"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_identity_derives_lineage_from_the_parent() {
        let root = SessionIdentity::root(SessionId::new());
        let child = root.child();
        assert_eq!(child.root_id(), root.id());
        assert_eq!(child.parent_id(), Some(root.id()));

        let grandchild = child.child();
        assert_eq!(grandchild.root_id(), root.id());
        assert_eq!(grandchild.parent_id(), Some(child.id()));
        assert!(root.is_root());
        assert!(!child.is_root());
        assert!(!grandchild.is_root());
    }

    #[test]
    fn invalid_identity_combinations_are_rejected() {
        let id = SessionId::new();
        let other = SessionId::new();

        assert_eq!(SessionIdentity::try_from_parts(id, other, None), None);
        assert_eq!(SessionIdentity::try_from_parts(id, id, Some(other)), None);
        assert_eq!(SessionIdentity::try_from_parts(id, other, Some(id)), None);
    }
}
