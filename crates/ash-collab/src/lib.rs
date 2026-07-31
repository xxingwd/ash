pub mod control;

pub use control::{
    install_subagent_tools, AgentControl, AgentRole, AgentSnapshot, AgentSpawner, AgentStatus,
    ChildAgent, SpawnRequest,
};
