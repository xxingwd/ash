pub mod control;
mod event;
mod snapshot;

pub use control::{install_collaboration, AgentControl};
pub use event::{SubagentEvent, SubagentEventKind};
pub use snapshot::{SubagentSnapshot, SubagentState};
