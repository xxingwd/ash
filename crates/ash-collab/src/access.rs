#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Root,
    Manager,
    Worker {
        member: bool,
        children: bool,
        groups: bool,
    },
}

pub(crate) const COLLAB_TOOLS: &[&str] =
    &["agent", "group", "workflow", "message", "history", "list"];

impl Access {
    pub(crate) fn member(self) -> bool {
        matches!(self, Self::Worker { member: true, .. })
    }

    pub(crate) fn coordinates(self) -> bool {
        matches!(
            self,
            Self::Root
                | Self::Manager
                | Self::Worker { children: true, .. }
                | Self::Worker { groups: true, .. }
        )
    }

    pub(crate) fn allows(self, tool: &str) -> bool {
        match tool {
            "agent" | "group" => self == Self::Root,
            "workflow" => self == Self::Manager,
            "message" | "list" => self.coordinates() || self.member(),
            #[cfg(test)]
            "wait" => self.coordinates(),
            "history" => {
                matches!(
                    self,
                    Self::Root | Self::Manager | Self::Worker { groups: true, .. }
                ) || self.member()
            }
            _ => false,
        }
    }
}
