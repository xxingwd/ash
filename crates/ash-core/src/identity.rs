use serde::{Deserialize, Serialize};

use crate::SessionId;

/// The durable identity of one session inside its collaboration tree.
///
/// The root session satisfies `root_id == id` and `parent_id == None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub id: SessionId,
    pub root_id: SessionId,
    pub parent_id: Option<SessionId>,
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
    pub fn is_root(self) -> bool {
        self.parent_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_identity_derives_lineage_from_the_parent() {
        let root = SessionIdentity::root(SessionId::new());
        let child = root.child();
        assert_eq!(child.root_id, root.id);
        assert_eq!(child.parent_id, Some(root.id));

        let grandchild = child.child();
        assert_eq!(grandchild.root_id, root.id);
        assert_eq!(grandchild.parent_id, Some(child.id));
        assert!(root.is_root());
        assert!(!child.is_root());
        assert!(!grandchild.is_root());
    }
}
