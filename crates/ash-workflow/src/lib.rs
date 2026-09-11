use ash_agent::{install_skills, Agent, Profile, Skill};
use ash_collab::{Blueprint, Definition, WeakControl};
use ash_core::{define_tool_with_timeout, Tool, ToolError, ToolTimeout};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{path::Path, sync::Arc};

#[cfg(test)]
mod tests;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkflowArgs {
    blueprint: Blueprint,
}

pub fn manager_input(task: &str, receipt: &str) -> String {
    format!(
        "The user requested this workflow task:\n{task}\n\n\
         The workflow launcher already created its manager using the agent tool path. \
         Creation receipt:\n{receipt}\n\n\
         Do not create another manager. Use wait with this manager's agent_id, not group_id. \
         The manager owns its groups; ask it to collect them rather than querying its groups directly. \
         If its reply is only a progress update, or leaves work running or unread, message this same \
         manager to continue collecting its results, then wait again. If it repeatedly returns the \
         same unfinished update without progress, report the pending work rather than endlessly \
         resending the same instruction. Do not treat a submission \
         receipt or a preliminary reply as the completed task."
    )
}

pub fn definition(base: Agent, skills: &[Skill]) -> Result<Definition, ToolError> {
    let profile =
        Profile::parse("workflow", include_str!("../agents/workflow.md")).map_err(error)?;
    let skill = Skill::from_markdown(
        Path::new("builtin/workflow.md"),
        include_str!("../skills/workflow/SKILL.md"),
    )
    .map_err(error)?;
    let mut skills = skills
        .iter()
        .filter(|skill| skill.name != "workflow")
        .cloned()
        .collect::<Vec<_>>();
    skills.push(skill);
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    let agent = base.with_profile(profile).map_err(error)?;
    Ok(Definition {
        agent: install_skills(agent, skills, None)?,
        capabilities: Some(tools),
    })
}

fn tools(control: WeakControl) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
    Ok(vec![define_tool_with_timeout("workflow", "Validate and instantiate your complete organization blueprint. Creates unstarted children and groups; never runs their tasks.", ToolTimeout::Disabled,
        move |context,args:WorkflowArgs| { let control=control.clone(); async move { control.upgrade()?.assemble(context,args.blueprint).await } })?])
}

fn error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}
