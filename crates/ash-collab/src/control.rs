use crate::{
    access::{Access, COLLAB_TOOLS},
    api::*,
    state::*,
    store, SubagentEvent, SubagentEventKind, SubagentState,
};
use ash_agent::{Agent, AgentSnapshot, Runtime};
#[cfg(test)]
use ash_core::ToolOutput;
use ash_core::{
    define_tool_with_timeout, with_tool_instructions, CancellationToken, SessionId,
    SessionIdentity, Tool, ToolContext, ToolError, ToolTimeout,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Weak},
};
use tokio::sync::{broadcast, Mutex, Notify};

pub type CapabilityFactory = fn(WeakControl) -> Result<Vec<Arc<dyn Tool>>, ToolError>;
pub struct Definition {
    pub agent: Agent,
    pub capabilities: Option<CapabilityFactory>,
}

#[derive(Clone)]
pub struct AgentControl {
    inner: Arc<ControlInner>,
}
#[derive(Clone)]
pub struct WeakControl {
    inner: Weak<ControlInner>,
}
struct ControlInner {
    runtime: Runtime,
    definitions: BTreeMap<String, Definition>,
    trees: Mutex<BTreeMap<SessionId, Tree>>,
    updates: Notify,
    events: broadcast::Sender<SubagentEvent>,
}

impl WeakControl {
    pub fn upgrade(&self) -> Result<AgentControl, ToolError> {
        self.inner
            .upgrade()
            .map(|inner| AgentControl { inner })
            .ok_or_else(|| store::error("collaboration runtime is closed"))
    }
}

impl AgentControl {
    pub async fn create(&self, context: ToolContext, args: AgentArgs) -> Result<String, ToolError> {
        let control = self.clone();
        run_command(async move { control.create_inner(context, args).await }).await
    }
    pub async fn group(&self, context: ToolContext, args: GroupArgs) -> Result<String, ToolError> {
        let control = self.clone();
        run_command(async move { control.group_inner(context, args).await }).await
    }
    pub async fn message(
        &self,
        context: ToolContext,
        args: MessageArgs,
    ) -> Result<String, ToolError> {
        let control = self.clone();
        run_command(async move { control.message_inner(context, args).await }).await
    }
    pub async fn assemble(
        &self,
        context: ToolContext,
        blueprint: Blueprint,
    ) -> Result<String, ToolError> {
        let control = self.clone();
        run_command(async move { control.assemble_inner(context, blueprint).await }).await
    }
    pub fn new(runtime: Runtime, definitions: Vec<Definition>) -> Result<Self, ToolError> {
        let mut catalog = BTreeMap::new();
        for definition in definitions {
            let name = definition
                .agent
                .profile()
                .ok_or_else(|| store::error("definition requires a profile"))?
                .name()
                .to_string();
            if catalog.insert(name.clone(), definition).is_some() {
                return Err(store::error(format!("duplicate profile: {name}")));
            }
        }
        if !catalog.contains_key("default") {
            return Err(store::error("default profile is required"));
        }
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(ControlInner {
                runtime,
                definitions: catalog,
                trees: Mutex::new(BTreeMap::new()),
                updates: Notify::new(),
                events,
            }),
        })
    }

    pub fn downgrade(&self) -> WeakControl {
        WeakControl {
            inner: Arc::downgrade(&self.inner),
        }
    }
    pub fn events(&self) -> broadcast::Receiver<SubagentEvent> {
        self.inner.events.subscribe()
    }
    fn directory(&self) -> PathBuf {
        self.inner.runtime.session_directory()
    }

    pub fn install_root(&self, base: Agent) -> Result<Agent, ToolError> {
        base.restrict_tools(COLLAB_TOOLS)
            .installing_tools(self.tools(Access::Root)?)
    }

    fn definition(&self, name: &str, access: Access) -> Result<Agent, ToolError> {
        let definition = self.inner.definitions.get(name).ok_or_else(|| {
            store::error(format!(
                "unknown profile: {name}; available: {}",
                self.inner
                    .definitions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        let mut agent = definition
            .agent
            .clone()
            .restrict_tools(COLLAB_TOOLS)
            .installing_tools(self.tools(access)?)?;
        if access == Access::Manager {
            if let Some(factory) = definition.capabilities {
                let catalog = self.profile_catalog();
                agent =
                    agent.installing_tools(factory(self.downgrade())?.into_iter().map(|tool| {
                        let instructions =
                            format!("{}\n\n{catalog}", tool.instructions().unwrap_or_default());
                        with_tool_instructions(tool, instructions)
                    }))?;
            }
        }
        Ok(agent)
    }

    fn restore_definition(
        &self,
        snapshot: &AgentSnapshot,
        access: Access,
    ) -> Result<Agent, ToolError> {
        let name = snapshot
            .profile
            .as_ref()
            .ok_or_else(|| store::error("stored child lacks profile"))?
            .name();
        let base = self.definition(name, access)?;
        let snapshot = snapshot.clone().without_tools(COLLAB_TOOLS);
        base.restore(&snapshot)
            .map_err(store::error)?
            .installing_tools(
                base.tools()
                    .iter()
                    .filter(|tool| COLLAB_TOOLS.contains(&tool.name()))
                    .cloned(),
            )
    }

    fn access(&self, profile: &str, member: bool, children: bool, groups: bool) -> Access {
        if !member
            && self
                .inner
                .definitions
                .get(profile)
                .is_some_and(|definition| definition.capabilities.is_some())
        {
            Access::Manager
        } else {
            Access::Worker {
                member,
                children,
                groups,
            }
        }
    }

    fn node_access(&self, tree: &Tree, node: &Node) -> Result<Access, ToolError> {
        let profile = node
            .definition
            .profile
            .as_ref()
            .ok_or_else(|| store::error("stored child lacks profile"))?
            .name();
        Ok(self.access(
            profile,
            node.group_id.is_some(),
            tree.nodes
                .values()
                .any(|child| child.identity.parent_id() == Some(node.identity.id())),
            tree.groups
                .values()
                .any(|group| group.owner == node.identity.id()),
        ))
    }

    fn profile_catalog(&self) -> String {
        let profiles = self
            .inner
            .definitions
            .iter()
            .map(|(name, definition)| {
                format!(
                    "- {name}: {}",
                    definition
                        .agent
                        .profile()
                        .map(|profile| profile.description())
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("Available agent profiles (default: default):\n{profiles}")
    }

    pub(crate) fn tools(&self, access: Access) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let mut tools = Vec::new();
        if access == Access::Root {
            let weak = self.downgrade();
            let tool = define_tool_with_timeout(
                "agent",
                "Create a child using a profile. Omit prompt to leave it unstarted.",
                ToolTimeout::Disabled,
                move |ctx, args: AgentArgs| {
                    let weak = weak.clone();
                    async move { weak.upgrade()?.create(ctx, args).await }
                },
            )?;
            tools.push(with_tool_instructions(
                tool,
                format!("Creating an agent without prompt succeeds with created=true and started=false. Keep its ID; do not create another instance merely because it has not started. Ordinary children cannot create descendants.\n\n{}", self.profile_catalog()),
            ));
            let weak = self.downgrade();
            tools.push(define_tool_with_timeout(
                "group",
                "Get or create an owned group; optionally join one unstarted direct child.",
                ToolTimeout::Disabled,
                move |ctx, args: GroupArgs| {
                    let weak = weak.clone();
                    async move { weak.upgrade()?.group(ctx, args).await }
                },
            )?);
        }
        let weak = self.downgrade();
        let message = define_tool_with_timeout("message", "Send work to a direct child or register the next same-group member. Running work is interrupted. Completion results are delivered automatically.", ToolTimeout::Disabled,
            move |ctx, args: MessageArgs| { let weak = weak.clone(); async move { weak.upgrade()?.message(ctx, args).await } })?;
        let instructions = [
            access.coordinates().then_some(include_str!("../prompt.md")),
            access.member().then_some(include_str!("../member.md")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
        tools.push(with_tool_instructions(message, instructions));
        let weak = self.downgrade();
        tools.push(define_tool_with_timeout("history", "Read shared group chat, latest 10 entries by default. Owners specify group_id; members may omit it.", ToolTimeout::Disabled,
            move |ctx, args: HistoryArgs| { let weak = weak.clone(); async move { weak.upgrade()?.history(ctx, args).await } })?);
        let weak = self.downgrade();
        tools.push(define_tool_with_timeout("list", "List direct child work lines and your own group, or inspect one visible group. Does not receive results.", ToolTimeout::Disabled,
            move |ctx, args: ListArgs| { let weak = weak.clone(); async move { weak.upgrade()?.list(ctx, args).await } })?);
        #[cfg(test)]
        {
            let weak = self.downgrade();
            let wait = define_tool_with_timeout(
                "wait",
                "Test-only synchronization primitive.",
                ToolTimeout::Disabled,
                move |ctx, args: WaitArgs| {
                    let weak = weak.clone();
                    async move { weak.upgrade()?.wait(ctx, args).await }
                },
            )?;
            tools.push(Arc::new(TestWaitTool {
                tool: wait,
                control: self.downgrade(),
            }));
        }
        tools.retain(|tool| access.allows(tool.name()));
        Ok(tools)
    }

    async fn ensure_tree(&self, identity: SessionIdentity) -> Result<(), ToolError> {
        let mut trees = self.inner.trees.lock().await;
        if trees.contains_key(&identity.root_id()) {
            return Ok(());
        }
        let mut tree = store::load(&self.directory(), identity.root_id())
            .await?
            .unwrap_or_default();
        validate_tree(&tree, identity.root_id())?;
        let incomplete = tree
            .nodes
            .values()
            .filter_map(|node| node.line.execution.as_ref())
            .chain(
                tree.groups
                    .values()
                    .filter_map(|group| group.line.execution.as_ref()),
            )
            .map(|active| active.delivery.receiver)
            .collect::<BTreeSet<_>>();
        let access = tree
            .nodes
            .iter()
            .map(|(id, node)| Ok((*id, self.node_access(&tree, node)?)))
            .collect::<Result<BTreeMap<_, _>, ToolError>>()?;
        for node in tree.nodes.values_mut() {
            let definition =
                self.restore_definition(&node.definition, access[&node.identity.id()])?;
            node.session = match self
                .inner
                .runtime
                .restore_child(&definition, node.identity)
                .await
                .map_err(store::error)?
            {
                Some(session) => Some(session),
                None if !node.started || incomplete.contains(&node.identity.id()) => {
                    Some(self.inner.runtime.start_at(&definition, node.identity))
                }
                None => {
                    return Err(store::error(format!(
                        "cannot restore completed child {}: its session record is missing",
                        node.identity.id()
                    )))
                }
            };
            self.recover_line(
                &mut node.line,
                node.identity
                    .parent_id()
                    .ok_or_else(|| store::error("missing parent"))?,
            )
            .await?;
        }
        for group in tree.groups.values_mut() {
            store::repair_chat(&self.directory(), identity.root_id(), &group.id).await?;
            let interrupted = group.line.execution.is_some();
            self.recover_line(&mut group.line, group.owner).await?;
            if interrupted {
                if let Some(completion) = &group.line.result {
                    self.log_completion(
                        identity.root_id(),
                        &Target::Group(group.id.clone()),
                        completion,
                    )
                    .await?;
                }
            }
        }
        store::save(&self.directory(), identity.root_id(), &tree).await?;
        trees.insert(identity.root_id(), tree);
        Ok(())
    }

    async fn recover_line(&self, line: &mut Line, owner: SessionId) -> Result<(), ToolError> {
        if let Some(active) = line.execution.take() {
            line.result=Some(Completion::diagnostic(active.delivery.receiver,ExecutionStatus::Interrupted,"Execution was interrupted by runtime shutdown. No input or external side effect was replayed. Inspect state before sending continuation."));
            line.unread = true;
        } else if !line.unread {
            if let Some(result) = &line.result {
                line.unread = !self
                    .inner
                    .runtime
                    .receipt_recorded(owner, &result.message_id)
                    .await
                    .map_err(store::error)?;
            }
        }
        Ok(())
    }

    pub async fn restore(&self, identity: SessionIdentity) -> Result<(), ToolError> {
        self.ensure_tree(identity).await?;
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        tree.closing = false;
        for node in tree.nodes.values().filter(|node| node.group_id.is_none()) {
            self.publish(
                identity.root_id(),
                node.identity.id(),
                if node.line.execution.is_some() {
                    SubagentState::Running
                } else {
                    SubagentState::Idle
                },
                tree,
            );
        }
        for group in tree.groups.values() {
            self.publish_group(identity.root_id(), group, tree);
        }
        Ok(())
    }

    async fn assemble_inner(
        &self,
        context: ToolContext,
        blueprint: Blueprint,
    ) -> Result<String, ToolError> {
        if blueprint.agents.len() > 64 || blueprint.groups.len() > 32 {
            return Err(store::error(
                "blueprint supports at most 64 agents and 32 groups",
            ));
        }
        self.ensure_tree(context.identity).await?;
        let root = context.identity.root_id();
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&root)
            .ok_or_else(|| store::error("missing tree"))?;
        if tree.closing {
            return Err(store::error("organization is closing"));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let manager = tree
            .nodes
            .get(&context.identity.id())
            .ok_or_else(|| store::error("workflow must be invoked by a manager child"))?;
        if manager.group_id.is_some() {
            return Err(store::error("group members cannot assemble organizations"));
        }
        let profile = manager
            .definition
            .profile
            .as_ref()
            .ok_or_else(|| store::error("missing manager profile"))?
            .name();
        if !self
            .inner
            .definitions
            .get(profile)
            .is_some_and(|definition| definition.capabilities.is_some())
        {
            return Err(store::error(
                "caller does not have workflow assembly capability",
            ));
        }
        let mut names = BTreeMap::new();
        for node in &blueprint.agents {
            nonempty(&node.name, "blueprint agent name")?;
            if node.name.len() > 128 || node.name.chars().any(char::is_control) {
                return Err(store::error("invalid blueprint agent name"));
            }
            if names.insert(node.name.clone(), node).is_some() {
                return Err(store::error("duplicate blueprint agent name"));
            }
        }
        let mut membership = BTreeMap::new();
        let mut group_names = BTreeSet::new();
        for group in &blueprint.groups {
            nonempty(&group.name, "blueprint group name")?;
            if group.name.len() > 128
                || group.name.chars().any(char::is_control)
                || group.prompt.len() > 65_536
            {
                return Err(store::error(
                    "invalid group name or prompt exceeding 64 KiB",
                ));
            }
            if !group_names.insert(group.name.clone()) {
                return Err(store::error("duplicate blueprint group name"));
            }
            if group
                .owner
                .as_ref()
                .is_some_and(|owner| !names.contains_key(owner))
            {
                return Err(store::error("unknown blueprint group owner"));
            }
            for member in &group.members {
                let node = names
                    .get(member)
                    .ok_or_else(|| store::error("unknown blueprint group member"))?;
                if node.parent != group.owner {
                    return Err(store::error(
                        "group members must be direct children of their owner",
                    ));
                }
                if membership
                    .insert(member.clone(), group.name.clone())
                    .is_some()
                {
                    return Err(store::error(
                        "blueprint agent belongs to more than one group",
                    ));
                }
            }
        }
        let mut identities = BTreeMap::new();
        let mut pending = names.clone();
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .filter(|(_, node)| {
                    node.parent
                        .as_ref()
                        .is_none_or(|parent| identities.contains_key(parent))
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return Err(store::error(
                    "blueprint contains a parent cycle or unknown parent",
                ));
            }
            for name in ready {
                let node = pending
                    .remove(&name)
                    .ok_or_else(|| store::error("missing blueprint node"))?;
                let parent = node
                    .parent
                    .as_ref()
                    .and_then(|parent| identities.get(parent))
                    .copied()
                    .unwrap_or(context.identity);
                identities.insert(name, parent.child());
            }
        }
        let group_ids = blueprint
            .groups
            .iter()
            .map(|group| (group.name.clone(), SessionId::new().to_string()))
            .collect::<BTreeMap<_, _>>();
        let mut staged = Tree::default();
        for node in &blueprint.agents {
            let identity = identities[&node.name];
            let group_id = membership
                .get(&node.name)
                .map(|name| group_ids[name].clone());
            let access = self.access(
                &node.profile,
                group_id.is_some(),
                blueprint
                    .agents
                    .iter()
                    .any(|child| child.parent.as_ref() == Some(&node.name)),
                blueprint
                    .groups
                    .iter()
                    .any(|group| group.owner.as_ref() == Some(&node.name)),
            );
            let definition = self.definition(&node.profile, access)?;
            staged.nodes.insert(
                identity.id(),
                Node {
                    identity,
                    definition: definition.snapshot(),
                    group_id,
                    started: false,
                    line: Line::default(),
                    session: Some(self.inner.runtime.start_at(&definition, identity)),
                },
            );
        }
        for group in &blueprint.groups {
            let id = group_ids[&group.name].clone();
            let owner = group
                .owner
                .as_ref()
                .map(|owner| identities[owner].id())
                .unwrap_or(context.identity.id());
            if tree
                .groups
                .values()
                .any(|existing| existing.owner == owner && existing.name == group.name)
            {
                return Err(store::error(
                    "owned group name already exists; reuse its group_id instead of rebuilding it",
                ));
            }
            staged.groups.insert(
                id.clone(),
                Group {
                    id,
                    owner,
                    name: group.name.clone(),
                    members: group
                        .members
                        .iter()
                        .map(|member| identities[member].id())
                        .collect(),
                    line: Line::default(),
                },
            );
        }
        let mut created_groups = Vec::new();
        let preparation = async {
            for group in &blueprint.groups {
                let id = &group_ids[&group.name];
                store::create_group(&self.directory(), root, id).await?;
                created_groups.push(id.clone());
                let path = store::group_directory(&self.directory(), root, id).join("prompt.md");
                store::write_prompt(&path, &group.prompt).await?;
            }
            Ok::<_, ToolError>(())
        }
        .await;
        if let Err(error) = preparation {
            for id in &created_groups {
                let _ =
                    tokio::fs::remove_dir_all(store::group_directory(&self.directory(), root, id))
                        .await;
            }
            return Err(error);
        }
        tree.nodes.append(&mut staged.nodes);
        tree.groups.append(&mut staged.groups);
        let agents = identities
            .iter()
            .map(|(name, identity)| (name.clone(), identity.id()))
            .collect::<BTreeMap<_, _>>();
        let groups = group_ids
            .iter()
            .map(|(name, id)| (name.clone(), self.group_value(root, &tree.groups[id], tree)))
            .collect::<BTreeMap<_, _>>();
        let output = json!({"manager_id":context.identity.id(),"agents":agents,"groups":groups,"assembled":true});
        tree.blueprints
            .push(json!({"blueprint":blueprint,"assembly":output}));
        if let Err(error) = store::save(&self.directory(), root, tree).await {
            tree.blueprints.pop();
            for id in identities.values() {
                tree.nodes.remove(&id.id());
            }
            for id in group_ids.values() {
                tree.groups.remove(id);
                let _ =
                    tokio::fs::remove_dir_all(store::group_directory(&self.directory(), root, id))
                        .await;
            }
            return Err(error);
        }
        for id in identities
            .values()
            .filter(|identity| tree.nodes[&identity.id()].group_id.is_none())
        {
            self.publish(root, id.id(), SubagentState::Idle, tree);
        }
        for id in group_ids.values() {
            self.publish_group(root, &tree.groups[id], tree);
        }
        encode(output)
    }

    async fn create_inner(
        &self,
        context: ToolContext,
        args: AgentArgs,
    ) -> Result<String, ToolError> {
        if let Some(prompt) = &args.prompt {
            nonempty(prompt, "prompt")?;
        }
        self.ensure_tree(context.identity).await?;
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&context.identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        require_root(tree, context.identity)?;
        if context.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let definition = self.definition(
            &args.profile,
            self.access(&args.profile, false, false, false),
        )?;
        let identity = context.identity.child();
        tree.nodes.insert(
            identity.id(),
            Node {
                identity,
                definition: definition.snapshot(),
                group_id: None,
                started: false,
                line: Line::default(),
                session: Some(self.inner.runtime.start_at(&definition, identity)),
            },
        );
        if let Err(error) = store::save(&self.directory(), identity.root_id(), tree).await {
            tree.nodes.remove(&identity.id());
            return Err(error);
        }
        self.publish(identity.root_id(), identity.id(), SubagentState::Idle, tree);
        if let Some(message) = args.prompt {
            let delivery = Delivery {
                id: SessionId::new().to_string(),
                sender: context.identity.id(),
                receiver: identity.id(),
                message,
            };
            self.start_locked(
                tree,
                identity.root_id(),
                Target::Agent(identity.id()),
                delivery,
                true,
            )
            .await?;
        }
        encode(
            json!({"agent_id":identity.id(),"profile":args.profile,"parent_id":context.identity.id(),"created":true,"started":tree.nodes[&identity.id()].started}),
        )
    }

    async fn group_inner(
        &self,
        context: ToolContext,
        args: GroupArgs,
    ) -> Result<String, ToolError> {
        nonempty(&args.group_id, "group_id")?;
        if args.group_id.chars().count() > 128 || args.group_id.chars().any(char::is_control) {
            return Err(store::error(
                "group name must be at most 128 characters without control characters",
            ));
        }
        self.ensure_tree(context.identity).await?;
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&context.identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        require_root(tree, context.identity)?;
        if context.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let existing = tree
            .groups
            .get(&args.group_id)
            .map(|group| group.id.clone())
            .or_else(|| {
                tree.groups
                    .values()
                    .find(|group| {
                        group.owner == context.identity.id() && group.name == args.group_id
                    })
                    .map(|group| group.id.clone())
            });
        if let Some(id) = &existing {
            if tree.groups[id].owner != context.identity.id() {
                return Err(store::error("group is not owned by caller"));
            }
            if args.agent_id.is_none()
                || args
                    .agent_id
                    .is_some_and(|agent| tree.groups[id].members.contains(&agent))
            {
                return encode(self.group_value(
                    context.identity.root_id(),
                    &tree.groups[id],
                    tree,
                ));
            }
            if tree.groups[id].line.execution.is_some() && args.agent_id.is_some() {
                return Err(store::error("cannot change running group membership"));
            }
        }
        if let Some(agent_id) = args.agent_id {
            let node = direct_child(tree, context.identity, agent_id)?;
            if node.started {
                return Err(store::error("only unstarted agents may join a group"));
            }
            if node.group_id.is_some() && node.group_id != existing {
                return Err(store::error("agent already belongs to another group"));
            }
        }
        let id = existing.unwrap_or_else(|| SessionId::new().to_string());
        let previous = tree.clone();
        if !tree.groups.contains_key(&id) {
            store::create_group(&self.directory(), context.identity.root_id(), &id).await?;
        }
        tree.groups.entry(id.clone()).or_insert_with(|| Group {
            id: id.clone(),
            name: args.group_id,
            owner: context.identity.id(),
            members: Vec::new(),
            line: Line::default(),
        });
        if let Some(agent_id) = args.agent_id {
            let children = tree
                .nodes
                .values()
                .any(|child| child.identity.parent_id() == Some(agent_id));
            let groups = tree.groups.values().any(|group| group.owner == agent_id);
            let node = tree
                .nodes
                .get_mut(&agent_id)
                .ok_or_else(|| store::error("missing child"))?;
            let definition = self.restore_definition(
                &node.definition,
                Access::Worker {
                    member: true,
                    children,
                    groups,
                },
            )?;
            node.definition = definition.snapshot();
            node.session = Some(self.inner.runtime.start_at(&definition, node.identity));
            node.group_id = Some(id.clone());
            let members = &mut tree
                .groups
                .get_mut(&id)
                .ok_or_else(|| store::error("missing group"))?
                .members;
            if !members.contains(&agent_id) {
                members.push(agent_id);
            }
        }
        if let Err(error) = store::save(&self.directory(), context.identity.root_id(), tree).await {
            *tree = previous;
            return Err(error);
        }
        if let Some(agent_id) = args.agent_id {
            let _ = self.inner.events.send(SubagentEvent {
                group_id: None,
                root_id: context.identity.root_id(),
                session_id: agent_id,
                name: label(tree, agent_id),
                kind: SubagentEventKind::Removed,
            });
        }
        self.publish_group(context.identity.root_id(), &tree.groups[&id], tree);
        encode(self.group_value(context.identity.root_id(), &tree.groups[&id], tree))
    }

    async fn message_inner(
        &self,
        context: ToolContext,
        args: MessageArgs,
    ) -> Result<String, ToolError> {
        nonempty(&args.message, "message")?;
        self.ensure_tree(context.identity).await?;
        let root = context.identity.root_id();
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&root)
            .ok_or_else(|| store::error("missing tree"))?;
        if tree.closing {
            return Err(store::error("organization is closing"));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let node = tree
            .nodes
            .get(&args.agent_id)
            .ok_or_else(|| store::error("target is not a visible child or group member"))?;
        let target = tree
            .target(args.agent_id)
            .ok_or_else(|| store::error("missing target"))?;
        let sibling = context.identity.id() != args.agent_id
            && node.group_id.is_some()
            && tree
                .nodes
                .get(&context.identity.id())
                .is_some_and(|caller| caller.group_id == node.group_id);
        if !sibling {
            direct_child(tree, context.identity, args.agent_id)?;
        }
        let delivery = Delivery {
            id: SessionId::new().to_string(),
            sender: context.identity.id(),
            receiver: args.agent_id,
            message: args.message,
        };
        let action;
        if sibling {
            let execution = tree
                .line(&target)
                .execution
                .as_ref()
                .ok_or_else(|| store::error("group has no active sender"))?;
            if execution.delivery.receiver != context.identity.id()
                || execution.replacement.is_some()
                || execution.successor.is_some()
            {
                return Err(store::error("only the current member may register one successor; replacement or successor already pending"));
            }
            self.log_delivery(root, &target, &delivery, "registered")
                .await?;
            tree.line_mut(&target)
                .execution
                .as_mut()
                .ok_or_else(|| store::error("missing execution"))?
                .successor = Some(delivery.clone());
            if let Err(error) = store::save(&self.directory(), root, tree).await {
                if let Some(active) = &mut tree.line_mut(&target).execution {
                    active.successor = None;
                }
                return Err(error);
            }
            action = "registered";
        } else if let Some(execution) = tree.line(&target).execution.as_ref() {
            if execution.replacement.is_some() {
                return Err(store::error(
                    "previous replacement is still settling; resend after it starts",
                ));
            }
            self.log_delivery(root, &target, &delivery, "replacement_accepted")
                .await?;
            let execution = tree
                .line_mut(&target)
                .execution
                .as_mut()
                .ok_or_else(|| store::error("missing execution"))?;
            let previous_successor = execution.successor.clone();
            execution.successor = None;
            execution.replacement = Some(delivery.clone());
            let cancellation = execution.cancellation.clone();
            if let Err(error) = store::save(&self.directory(), root, tree).await {
                if let Some(active) = &mut tree.line_mut(&target).execution {
                    active.replacement = None;
                    active.successor = previous_successor;
                }
                return Err(error);
            }
            cancellation.cancel();
            if let Some(active) = &tree.line(&target).execution {
                self.publish(root, active.delivery.receiver, SubagentState::Running, tree);
            }
            action = "replacement_accepted";
        } else {
            if tree.line(&target).unread {
                return Err(store::error(format!(
                    "receive the previous result with {} before sending more work",
                    wait_hint(&target)
                )));
            }
            self.start_locked(tree, root, target.clone(), delivery.clone(), true)
                .await?;
            action = "accepted";
        }
        encode(
            json!({"agent_id":args.agent_id,"group_id":group_id(&target),"action":action,"message_id":delivery.id}),
        )
    }

    async fn start_locked(
        &self,
        tree: &mut Tree,
        root: SessionId,
        target: Target,
        delivery: Delivery,
        new_message: bool,
    ) -> Result<(), ToolError> {
        if new_message {
            self.log_delivery(root, &target, &delivery, "accepted")
                .await?;
        }
        let execution_id = SessionId::new();
        let started = tree.nodes[&delivery.receiver].started;
        tree.line_mut(&target).execution = Some(Execution {
            started: false,
            failure: None,
            id: execution_id,
            delivery: delivery.clone(),
            successor: None,
            replacement: None,
            cancellation: CancellationToken::new(),
        });
        tree.nodes
            .get_mut(&delivery.receiver)
            .ok_or_else(|| store::error("missing recipient"))?
            .started = true;
        if let Err(error) = store::save(&self.directory(), root, tree).await {
            tree.line_mut(&target).execution = None;
            tree.nodes
                .get_mut(&delivery.receiver)
                .ok_or_else(|| store::error("missing recipient"))?
                .started = started;
            return Err(error);
        }
        let submission = async {
            let input = self.input(tree, root, &target, &delivery).await?;
            let session = tree.nodes[&delivery.receiver]
                .session
                .as_ref()
                .ok_or_else(|| store::error("recipient session unavailable"))?;
            let events = session.events();
            let mut turn = session.try_submit(input).map_err(store::error)?;
            turn.wait_started().await.map_err(store::error)?;
            Ok::<_, ToolError>((turn, events))
        }
        .await;
        match submission {
            Ok((turn, mut events)) => {
                let turn_id = turn.id();
                tree.line_mut(&target)
                    .execution
                    .as_mut()
                    .ok_or_else(|| store::error("missing execution"))?
                    .cancellation = turn.cancellation_token();
                tree.line_mut(&target)
                    .execution
                    .as_mut()
                    .ok_or_else(|| store::error("missing execution"))?
                    .started = true;
                let durable_start = async {
                    self.log_delivery(root, &target, &delivery, "started")
                        .await?;
                    store::save(&self.directory(), root, tree).await
                }
                .await;
                if let Err(error) = durable_start {
                    if let Some(active) = &mut tree.line_mut(&target).execution {
                        active.failure = Some(format!("Cannot persist execution start: {error}"));
                        active.cancellation.cancel();
                    }
                }
                self.publish(root, delivery.receiver, SubagentState::Running, tree);
                let weak = self.downgrade();
                let sender = delivery.receiver;
                let event_control = self.downgrade();
                let event_target = target.clone();
                let group_id = tree.nodes[&sender]
                    .group_id
                    .as_ref()
                    .and_then(|id| serde_json::from_value::<SessionId>(json!(id)).ok());
                tokio::spawn(async move {
                    while let Some(event) = events.next().await {
                        if let Ok(event) = event {
                            let finished = matches!(&event, ash_core::SessionEvent::Finished { turn, .. } if turn.id == turn_id)
                                || matches!(&event, ash_core::SessionEvent::Discarded { turn_id: id, .. } if *id == turn_id);
                            if !finished || group_id.is_none() {
                                let Ok(control) = event_control.upgrade() else {
                                    break;
                                };
                                control
                                    .forward_event(root, &event_target, execution_id, sender, event)
                                    .await;
                            }
                            if finished {
                                break;
                            }
                        }
                    }
                });
                tokio::spawn(async move {
                    let completion = match turn.wait().await {
                        Ok(turn) => Completion::from_turn(sender, Ok(&turn)),
                        Err(ash_core::AshError::Cancelled) => Completion::diagnostic(
                            sender,
                            ExecutionStatus::Cancelled,
                            "Agent was cancelled; existing side effects were not rolled back.",
                        ),
                        Err(error) => Completion::from_turn(sender, Err(error.to_string())),
                    };
                    if let Ok(control) = weak.upgrade() {
                        control.finish(root, target, execution_id, completion).await;
                    }
                });
            }
            Err(error) => {
                let completion = Completion::diagnostic(
                    delivery.receiver,
                    ExecutionStatus::Failed,
                    format!("Recipient could not start: {error}"),
                );
                self.settle(tree, root, &target, completion).await?;
            }
        }
        Ok(())
    }

    async fn forward_event(
        &self,
        root: SessionId,
        target: &Target,
        execution_id: SessionId,
        sender: SessionId,
        event: ash_core::SessionEvent,
    ) {
        let trees = self.inner.trees.lock().await;
        let Some(tree) = trees.get(&root) else {
            return;
        };
        if tree
            .line(target)
            .execution
            .as_ref()
            .is_some_and(|active| active.id == execution_id)
        {
            let group_id =
                group_id(target).and_then(|id| serde_json::from_value::<SessionId>(json!(id)).ok());
            let _ = self.inner.events.send(SubagentEvent {
                group_id,
                root_id: root,
                session_id: sender,
                name: label(tree, sender),
                kind: SubagentEventKind::Session(event),
            });
        }
    }

    fn finish(
        &self,
        root: SessionId,
        target: Target,
        execution_id: SessionId,
        mut completion: Completion,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut trees = self.inner.trees.lock().await;
            let Some(tree) = trees.get_mut(&root) else {
                return;
            };
            if !tree
                .line(&target)
                .execution
                .as_ref()
                .is_some_and(|active| active.id == execution_id)
            {
                return;
            }
            let active = tree.line_mut(&target).execution.take();
            let Some(active) = active else {
                return;
            };
            if let Some(error) = active.failure {
                completion =
                    Completion::diagnostic(completion.sender_id, ExecutionStatus::Failed, error);
            }
            if let Err(error) = self.log_completion(root, &target, &completion).await {
                completion = Completion::diagnostic(
                    completion.sender_id,
                    ExecutionStatus::Failed,
                    format!("Cannot record group completion: {error}"),
                );
            }
            let next = active.replacement.or_else(|| {
                (completion.status == ExecutionStatus::Stopped)
                    .then_some(active.successor)
                    .flatten()
            });
            let result = if let Some(next) = next {
                self.start_locked(tree, root, target.clone(), next, false)
                    .await
            } else {
                let line = tree.line_mut(&target);
                line.result = Some(completion.clone());
                line.unread = true;
                store::save(&self.directory(), root, tree).await
            };
            if let Err(error) = result {
                let line = tree.line_mut(&target);
                line.execution = None;
                line.result = Some(Completion::diagnostic(
                    completion.sender_id,
                    ExecutionStatus::Failed,
                    format!("Collaboration persistence or scheduling failed: {error}"),
                ));
                line.unread = true;
                tracing::error!(%error, "collaboration stopped");
            }
            let parent_session = if tree.line(&target).execution.is_none() {
                let owner = match &target {
                    Target::Agent(id) => tree
                        .nodes
                        .get(id)
                        .and_then(|node| node.identity.parent_id()),
                    Target::Group(id) => tree.groups.get(id).map(|group| group.owner),
                };
                owner.and_then(|id| tree.nodes.get(&id).and_then(|node| node.session.clone()))
            } else {
                None
            };
            if tree.line(&target).execution.is_none() {
                self.publish(root, completion.sender_id, SubagentState::Idle, tree);
            }
            self.inner.updates.notify_waiters();
            drop(trees);

            if let Some(session) = parent_session {
                let target = match target {
                    Target::Agent(id) => format!("agent {id}"),
                    Target::Group(id) => format!("group {id}"),
                };
                let notice = format!(
                    "<collaboration_completion>\nTarget: {target}\nStatus: {:?}\nResult: {}\n</collaboration_completion>",
                    completion.status, completion.message
                );
                tokio::spawn(async move {
                    if let Ok(turn) = session.try_submit(notice) {
                        let _ = turn.wait().await;
                    }
                });
            }
        })
    }

    async fn settle(
        &self,
        tree: &mut Tree,
        root: SessionId,
        target: &Target,
        mut completion: Completion,
    ) -> Result<(), ToolError> {
        if let Err(error) = self.log_completion(root, target, &completion).await {
            completion = Completion::diagnostic(
                completion.sender_id,
                ExecutionStatus::Failed,
                format!("Cannot append diagnostic: {error}"),
            );
        }
        let line = tree.line_mut(target);
        line.execution = None;
        line.result = Some(completion);
        line.unread = true;
        let result = store::save(&self.directory(), root, tree).await;
        if let Some(completion) = &tree.line(target).result {
            self.publish(root, completion.sender_id, SubagentState::Idle, tree);
        }
        self.inner.updates.notify_waiters();
        result
    }

    async fn input(
        &self,
        tree: &Tree,
        root: SessionId,
        target: &Target,
        delivery: &Delivery,
    ) -> Result<String, ToolError> {
        let node = &tree.nodes[&delivery.receiver];
        let children = tree.nodes.values().filter(|child| child.identity.parent_id() == Some(node.identity.id())).map(|child|json!({"agent_id":child.identity.id(),"profile":child.definition.profile.as_ref().map(|profile|profile.name()),"group_id":child.group_id})).collect::<Vec<_>>();
        let child_groups = tree
            .groups
            .values()
            .filter(|group| group.owner == node.identity.id())
            .map(|group| json!({"group_id":group.id,"name":group.name,"members":group.members}))
            .collect::<Vec<_>>();
        let group = if let Target::Group(id) = target {
            let path = store::group_directory(&self.directory(), root, id).join("prompt.md");
            let prompt = tokio::fs::read_to_string(&path)
                .await
                .map_err(store::error)?;
            if prompt.len() > 65_536 {
                return Err(store::error(
                    "group prompt exceeds 64 KiB; shorten prompt.md",
                ));
            }
            let members=tree.groups[id].members.iter().map(|id|json!({"agent_id":id,"profile":tree.nodes[id].definition.profile.as_ref().map(|profile|profile.name())})).collect::<Vec<_>>();
            json!({"group_id":id,"members":members,"prompt_path":path,"prompt":prompt})
        } else {
            Value::Null
        };
        Ok(format!(
            "<collaboration_context>\n{}\n</collaboration_context>\n\n{}",
            json!({"agent_id":node.identity.id(),"parent_id":node.identity.parent_id(),"children":children,"child_groups":child_groups,"group":group,"sender_id":delivery.sender,"message_id":delivery.id}),
            delivery.message
        ))
    }

    async fn log_delivery(
        &self,
        root: SessionId,
        target: &Target,
        delivery: &Delivery,
        kind: &str,
    ) -> Result<(), ToolError> {
        if let Target::Group(id) = target {
            store::append(
                &self.directory(),
                root,
                id,
                &ChatEntry {
                    message_id: if kind == "started" {
                        SessionId::new().to_string()
                    } else {
                        delivery.id.clone()
                    },
                    source: if kind == "started" {
                        MessageSource::Runtime
                    } else {
                        MessageSource::Agent
                    },
                    sender_id: delivery.sender,
                    receiver_id: Some(delivery.receiver),
                    kind: kind.into(),
                    message: if kind == "started" {
                        String::new()
                    } else {
                        delivery.message.clone()
                    },
                    delivery_id: Some(delivery.id.clone()),
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn log_completion(
        &self,
        root: SessionId,
        target: &Target,
        completion: &Completion,
    ) -> Result<(), ToolError> {
        if let Target::Group(id) = target {
            store::append(
                &self.directory(),
                root,
                id,
                &ChatEntry {
                    message_id: completion.message_id.clone(),
                    source: completion.source,
                    sender_id: completion.sender_id,
                    receiver_id: None,
                    kind: "completion".into(),
                    message: completion.message.clone(),
                    delivery_id: None,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub async fn wait(&self, context: ToolContext, args: WaitArgs) -> Result<String, ToolError> {
        self.ensure_tree(context.identity).await?;
        loop {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let trees = self.inner.trees.lock().await;
                let tree = trees
                    .get(&context.identity.root_id())
                    .ok_or_else(|| store::error("missing tree"))?;
                let target = wait_target(tree, context.identity, &args)?;
                if tree.line(&target).execution.is_none() {
                    let owners = match &target {
                        Target::Agent(id) => vec![*id],
                        Target::Group(id) => tree.groups[id].members.clone(),
                    };
                    let mut pending = pending_work(tree, &owners);
                    let pending_truncated = pending.len() > 32;
                    pending.truncate(32);
                    let message_limit = 60_000usize
                        .saturating_sub(serde_json::to_vec(&pending).map_err(store::error)?.len());
                    let mut result = tree.line(&target).result.clone();
                    let mut truncated = false;
                    if let Some(completion) = &mut result {
                        while serde_json::to_vec(completion).map_err(store::error)?.len()
                            > message_limit
                        {
                            let limit = completion.message.len() / 2;
                            truncate(&mut completion.message, limit);
                            truncated = true;
                        }
                    }
                    return encode(
                        json!({"agent_id":agent_id(&target),"group_id":group_id(&target),"result":result,"state":"idle","pending":pending,"pending_truncated":pending_truncated,"truncated":truncated}),
                    );
                }
            }
            context.run(notified).await?;
        }
    }

    #[cfg(test)]
    async fn received(
        &self,
        context: ToolContext,
        args: WaitArgs,
        output: &ToolOutput,
    ) -> Result<(), ToolError> {
        let value: Value = serde_json::from_str(&output.text).map_err(store::error)?;
        let Some(message_id) = value["result"]["message_id"].as_str() else {
            return Ok(());
        };
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&context.identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        let target = wait_target(tree, context.identity, &args)?;
        let line = tree.line_mut(&target);
        if line
            .result
            .as_ref()
            .is_some_and(|result| result.message_id == message_id)
        {
            line.unread = false;
            if let Err(error) =
                store::save(&self.directory(), context.identity.root_id(), tree).await
            {
                tree.line_mut(&target).unread = true;
                return Err(error);
            }
            if let Some(result) = &tree.line(&target).result {
                self.publish(
                    context.identity.root_id(),
                    result.sender_id,
                    SubagentState::Idle,
                    tree,
                );
            }
        }
        Ok(())
    }

    pub async fn history(
        &self,
        context: ToolContext,
        args: HistoryArgs,
    ) -> Result<String, ToolError> {
        let limit = args.limit.unwrap_or(10);
        if !(1..=100).contains(&limit) {
            return Err(store::error("history limit must be 1..100"));
        }
        self.ensure_tree(context.identity).await?;
        let trees = self.inner.trees.lock().await;
        let tree = trees
            .get(&context.identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        let id = visible_group(tree, context.identity, args.group_id.as_deref())?;
        let entries = store::history(&self.directory(), context.identity.root_id(), &id).await?;
        let end = match args.before {
            Some(before) => entries
                .iter()
                .position(|entry| entry.message_id == before)
                .ok_or_else(|| store::error("unknown history anchor for this group"))?,
            None => entries.len(),
        };
        let mut selected = Vec::new();
        let mut bytes = 0;
        let mut truncated = false;
        for entry in entries[..end].iter().rev().take(limit) {
            let mut entry = entry.clone();
            let length = serde_json::to_vec(&entry).map_err(store::error)?.len();
            if bytes + length > 60_000 {
                if !selected.is_empty() {
                    break;
                }
                while serde_json::to_vec(&entry).map_err(store::error)?.len() > 60_000 {
                    let limit = entry.message.len() / 2;
                    truncate(&mut entry.message, limit);
                }
                truncated = true;
            }
            bytes += serde_json::to_vec(&entry).map_err(store::error)?.len();
            selected.push(entry);
        }
        selected.reverse();
        let next_before = (end > selected.len())
            .then(|| selected.first().map(|entry| entry.message_id.clone()))
            .flatten();
        encode(
            json!({"group_id":id,"messages":selected,"next_before":next_before,"truncated":truncated}),
        )
    }

    pub async fn pending(&self, identity: SessionIdentity) -> Result<Vec<PendingWork>, ToolError> {
        self.ensure_tree(identity).await?;
        let trees = self.inner.trees.lock().await;
        let tree = trees
            .get(&identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        if !identity.is_root()
            && !tree
                .nodes
                .get(&identity.id())
                .is_some_and(|node| node.identity == identity)
        {
            return Err(store::error("caller is not in this organization"));
        }
        Ok(pending_work(tree, &[identity.id()]))
    }

    pub async fn collect_pending(
        &self,
        identity: SessionIdentity,
    ) -> Result<Option<String>, ToolError> {
        self.ensure_tree(identity).await?;
        let mut trees = self.inner.trees.lock().await;
        let tree = trees
            .get_mut(&identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        let pending = pending_work(tree, &[identity.id()]);
        let notice = pending_notice(&pending);
        let idle = pending
            .iter()
            .all(|work| work.state != PendingState::Running);
        let completion = if idle {
            pending
                .iter()
                .filter_map(|work| {
                    let result = match &work.target {
                        PendingTarget::Agent { agent_id } => {
                            tree.nodes.get(agent_id)?.line.result.as_ref()
                        }
                        PendingTarget::Group { group_id } => {
                            tree.groups.get(group_id)?.line.result.as_ref()
                        }
                    };
                    let result = result?;
                    Some(format!(
                        "target: {:?}\nstatus: {:?}\nresult: {}",
                        work.target, result.status, result.message
                    ))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let notice = if completion.is_empty() {
            notice
        } else {
            Some(format!(
                "<collaboration_completion>\n{}\n</collaboration_completion>",
                completion.join("\n\n")
            ))
        };
        if notice.is_some() && idle {
            for node in tree.nodes.values_mut() {
                if node.identity.parent_id() == Some(identity.id()) && node.line.execution.is_none()
                {
                    node.line.unread = false;
                }
            }
            for group in tree.groups.values_mut() {
                if group.owner == identity.id() && group.line.execution.is_none() {
                    group.line.unread = false;
                }
            }
            store::save(&self.directory(), identity.root_id(), tree).await?;
        }
        Ok(notice)
    }

    pub async fn list(&self, context: ToolContext, args: ListArgs) -> Result<String, ToolError> {
        self.ensure_tree(context.identity).await?;
        let trees = self.inner.trees.lock().await;
        let tree = trees
            .get(&context.identity.root_id())
            .ok_or_else(|| store::error("missing tree"))?;
        if let Some(id) = args.group_id {
            let id = visible_group(tree, context.identity, Some(&id))?;
            return encode(self.group_value(context.identity.root_id(), &tree.groups[&id], tree));
        }
        let agents = tree
            .nodes
            .values()
            .filter(|node| {
                node.identity.parent_id() == Some(context.identity.id()) && node.group_id.is_none()
            })
            .map(|node| node_value(node, &node.line))
            .collect::<Vec<_>>();
        let groups = tree
            .groups
            .values()
            .filter(|group| {
                group.owner == context.identity.id()
                    || group.members.contains(&context.identity.id())
            })
            .map(|group| self.group_value(context.identity.root_id(), group, tree))
            .collect::<Vec<_>>();
        encode(
            json!({"agent_id":context.identity.id(),"parent_id":context.identity.parent_id(),"agents":agents,"groups":groups,"pending":pending_work(tree, &[context.identity.id()])}),
        )
    }

    fn group_value(&self, root: SessionId, group: &Group, tree: &Tree) -> Value {
        let directory = store::group_directory(&self.directory(), root, &group.id);
        let members = group
            .members
            .iter()
            .filter_map(|id| tree.nodes.get(id))
            .map(|node| node_value(node, &group.line))
            .collect::<Vec<_>>();
        json!({"group_id":group.id,"name":group.name,"owner":group.owner,"members":members,"current_agent_id":group.line.execution.as_ref().map(|execution| execution.delivery.receiver),"state":line_state(&group.line),"unread":group.line.unread,"prompt_path":directory.join("prompt.md"),"chat_path":directory.join("chat.jsonl")})
    }

    fn publish(&self, root: SessionId, id: SessionId, state: SubagentState, tree: &Tree) {
        if let Some(group) = tree
            .nodes
            .get(&id)
            .and_then(|node| node.group_id.as_ref())
            .and_then(|id| tree.groups.get(id))
        {
            self.publish_group(root, group, tree);
            return;
        }
        let _ = self.inner.events.send(SubagentEvent {
            group_id: None,
            root_id: root,
            session_id: id,
            name: label(tree, id),
            kind: SubagentEventKind::StateChanged(state),
        });
    }

    fn publish_group(&self, root: SessionId, group: &Group, tree: &Tree) {
        let _ = self.inner.events.send(self.group_event(root, group, tree));
    }

    fn group_event(&self, root: SessionId, group: &Group, tree: &Tree) -> SubagentEvent {
        let group_id = serde_json::from_value::<SessionId>(json!(group.id)).ok();
        let current = group
            .line
            .execution
            .as_ref()
            .map(|active| active.delivery.receiver)
            .or_else(|| group.line.result.as_ref().map(|result| result.sender_id));
        let name = current
            .map(|id| label(tree, id))
            .unwrap_or_else(|| format!("group {}", group.name));
        SubagentEvent {
            group_id,
            root_id: root,
            session_id: current.unwrap_or(group.owner),
            name,
            kind: SubagentEventKind::StateChanged(if group.line.execution.is_some() {
                SubagentState::Running
            } else {
                SubagentState::Idle
            }),
        }
    }

    pub async fn snapshots(&self, root: SessionId) -> Result<Vec<SubagentEvent>, ToolError> {
        self.ensure_tree(SessionIdentity::root(root)).await?;
        let trees = self.inner.trees.lock().await;
        let tree = trees
            .get(&root)
            .ok_or_else(|| store::error("missing tree"))?;
        let mut snapshots = tree
            .nodes
            .values()
            .filter(|node| node.group_id.is_none())
            .map(|node| SubagentEvent {
                group_id: None,
                root_id: root,
                session_id: node.identity.id(),
                name: label(tree, node.identity.id()),
                kind: SubagentEventKind::StateChanged(if node.line.execution.is_some() {
                    SubagentState::Running
                } else {
                    SubagentState::Idle
                }),
            })
            .collect::<Vec<_>>();
        snapshots.extend(
            tree.groups
                .values()
                .map(|group| self.group_event(root, group, tree)),
        );
        Ok(snapshots)
    }

    pub async fn close(&self, root: SessionId) {
        {
            let mut trees = self.inner.trees.lock().await;
            if let Some(tree) = trees.get_mut(&root) {
                tree.closing = true;
                for line in tree
                    .nodes
                    .values_mut()
                    .map(|node| &mut node.line)
                    .chain(tree.groups.values_mut().map(|group| &mut group.line))
                {
                    if let Some(active) = &mut line.execution {
                        active.successor = None;
                        active.replacement = None;
                        active.cancellation.cancel();
                    }
                }
            }
        }
        self.wait_idle(root).await;
    }

    pub async fn wait_idle(&self, root: SessionId) {
        loop {
            let notified = self.inner.updates.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let trees = self.inner.trees.lock().await;
                if !trees.get(&root).is_some_and(|tree| {
                    tree.nodes
                        .values()
                        .any(|node| node.line.execution.is_some())
                        || tree
                            .groups
                            .values()
                            .any(|group| group.line.execution.is_some())
                }) {
                    break;
                }
            }
            notified.await;
        }
    }
}

#[cfg(test)]
struct TestWaitTool {
    tool: Arc<dyn Tool>,
    control: WeakControl,
}

#[cfg(test)]
#[async_trait::async_trait]
impl Tool for TestWaitTool {
    fn name(&self) -> &str {
        self.tool.name()
    }
    fn description(&self) -> &str {
        self.tool.description()
    }
    fn parameters_schema(&self) -> Value {
        self.tool.parameters_schema()
    }
    fn timeout(&self) -> ToolTimeout {
        ToolTimeout::Disabled
    }
    async fn execute(&self, context: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        self.tool.execute(context, args).await
    }
    async fn committed(
        &self,
        context: ToolContext,
        args: &Value,
        output: &ToolOutput,
    ) -> Result<(), ToolError> {
        self.control
            .upgrade()?
            .received(
                context,
                serde_json::from_value(args.clone()).map_err(store::error)?,
                output,
            )
            .await
    }
}

async fn run_command(
    operation: impl std::future::Future<Output = Result<String, ToolError>> + Send + 'static,
) -> Result<String, ToolError> {
    tokio::spawn(operation).await.map_err(store::error)?
}

fn pending_work(tree: &Tree, owners: &[SessionId]) -> Vec<PendingWork> {
    let within = |mut owner| loop {
        if owners.contains(&owner) {
            return true;
        }
        let Some(parent) = tree
            .nodes
            .get(&owner)
            .and_then(|node| node.identity.parent_id())
        else {
            return false;
        };
        owner = parent;
    };
    let state = |line: &Line| {
        if line.execution.is_some() {
            Some(PendingState::Running)
        } else if line.unread {
            Some(PendingState::Unread)
        } else {
            None
        }
    };
    let agents = tree
        .nodes
        .values()
        .filter(|node| node.group_id.is_none())
        .filter_map(|node| {
            let owner_id = node.identity.parent_id()?;
            within(owner_id).then_some(())?;
            Some(PendingWork {
                owner_id,
                target: PendingTarget::Agent {
                    agent_id: node.identity.id(),
                },
                state: state(&node.line)?,
            })
        });
    let groups = tree.groups.values().filter_map(|group| {
        within(group.owner).then_some(())?;
        Some(PendingWork {
            owner_id: group.owner,
            target: PendingTarget::Group {
                group_id: group.id.clone(),
            },
            state: state(&group.line)?,
        })
    });
    agents.chain(groups).collect()
}

fn require_root(tree: &Tree, identity: SessionIdentity) -> Result<(), ToolError> {
    if tree.closing {
        return Err(store::error("organization is closing"));
    }
    if identity.is_root() {
        return Ok(());
    }
    Err(store::error("only the root agent may create agents or groups; workflow managers use their blueprint tool"))
}
fn direct_child(tree: &Tree, identity: SessionIdentity, id: SessionId) -> Result<&Node, ToolError> {
    tree.nodes.get(&id).filter(|node| node.identity.parent_id() == Some(identity.id())).ok_or_else(|| store::error("message/wait target must be a direct child; return your final answer to ask your parent for help"))
}
fn visible_group(
    tree: &Tree,
    identity: SessionIdentity,
    requested: Option<&str>,
) -> Result<String, ToolError> {
    let id = requested
        .map(str::to_string)
        .or_else(|| {
            tree.nodes
                .get(&identity.id())
                .and_then(|node| node.group_id.clone())
        })
        .ok_or_else(|| store::error("specify group_id; only group members may omit it"))?;
    let group = tree
        .groups
        .get(&id)
        .filter(|group| group.owner == identity.id() || group.members.contains(&identity.id()))
        .ok_or_else(|| store::error("group is not visible to caller"))?;
    Ok(group.id.clone())
}
fn wait_target(
    tree: &Tree,
    identity: SessionIdentity,
    args: &WaitArgs,
) -> Result<Target, ToolError> {
    match (&args.agent_id, &args.group_id) {
        (Some(id), None) => {
            let node = direct_child(tree, identity, *id)?;
            if let Some(group) = &node.group_id {
                return Err(store::error(format!(
                    "cannot wait a group member; use wait(group_id={group})"
                )));
            }
            Ok(Target::Agent(*id))
        }
        (None, Some(id)) => {
            let id = visible_group(tree, identity, Some(id))?;
            if tree.groups[&id].owner != identity.id() {
                return Err(store::error("cannot wait your own group"));
            }
            Ok(Target::Group(id))
        }
        _ => Err(store::error(
            "wait requires exactly one of agent_id or group_id",
        )),
    }
}
fn validate_tree(tree: &Tree, root: SessionId) -> Result<(), ToolError> {
    for (id, node) in &tree.nodes {
        if *id == root
            || *id != node.identity.id()
            || node.identity.root_id() != root
            || node.identity.parent_id().is_none()
        {
            return Err(store::error("invalid persisted child identity"));
        }
        let mut ancestor = node.identity.parent_id();
        let mut seen = BTreeSet::new();
        while let Some(parent) = ancestor {
            if parent == root {
                break;
            }
            if !seen.insert(parent) {
                return Err(store::error("persisted parent cycle"));
            }
            ancestor = tree
                .nodes
                .get(&parent)
                .ok_or_else(|| store::error("missing persisted parent"))?
                .identity
                .parent_id();
        }
        if let Some(group) = &node.group_id {
            if !tree.groups.get(group).is_some_and(|group| {
                group.members.contains(id) && Some(group.owner) == node.identity.parent_id()
            }) {
                return Err(store::error("invalid persisted group membership"));
            }
        }
    }
    for (id, group) in &tree.groups {
        if id != &group.id || serde_json::from_value::<SessionId>(json!(id)).is_err() {
            return Err(store::error("invalid persisted group storage key"));
        }
        if group.owner != root && !tree.nodes.contains_key(&group.owner) {
            return Err(store::error("missing persisted group owner"));
        }
        if group.members.iter().collect::<BTreeSet<_>>().len() != group.members.len() {
            return Err(store::error("duplicate persisted group member"));
        }
        for member in &group.members {
            if !tree
                .nodes
                .get(member)
                .is_some_and(|node| node.group_id.as_ref() == Some(id))
            {
                return Err(store::error("missing persisted group member"));
            }
        }
    }
    for (target, line) in tree
        .nodes
        .iter()
        .filter(|(_, node)| node.group_id.is_none())
        .map(|(id, node)| (Target::Agent(*id), &node.line))
        .chain(
            tree.groups
                .iter()
                .map(|(id, group)| (Target::Group(id.clone()), &group.line)),
        )
    {
        if line.unread && line.result.is_none() {
            return Err(store::error("unread persisted line has no result"));
        }
        if let Some(active) = &line.execution {
            for delivery in std::iter::once(&active.delivery)
                .chain(active.successor.iter())
                .chain(active.replacement.iter())
            {
                if tree.target(delivery.receiver).as_ref() != Some(&target) {
                    return Err(store::error("invalid persisted execution recipient"));
                }
            }
        }
        if let Some(result) = &line.result {
            if tree.target(result.sender_id).as_ref() != Some(&target) {
                return Err(store::error("invalid persisted result sender"));
            }
        }
    }
    Ok(())
}
fn line_state(line: &Line) -> &'static str {
    match &line.execution {
        Some(active) if active.replacement.is_some() => "replacing",
        Some(_) => "running",
        None => "idle",
    }
}
fn node_value(node: &Node, line: &Line) -> Value {
    json!({"agent_id":node.identity.id(),"parent_id":node.identity.parent_id(),"profile":node.definition.profile.as_ref().map(|profile|profile.name()),"group_id":node.group_id,"state":if line.execution.as_ref().is_some_and(|active|active.delivery.receiver==node.identity.id()){line_state(line)}else{"idle"},"unread":line.unread,"started":node.started})
}
fn label(tree: &Tree, id: SessionId) -> String {
    tree.nodes
        .get(&id)
        .map(|node| {
            let role = node
                .definition
                .profile
                .as_ref()
                .map(|profile| profile.name())
                .unwrap_or("agent");
            node.group_id
                .as_ref()
                .map(|group| {
                    format!(
                        "group {} / {role} {}{}",
                        tree.groups[group].name,
                        id,
                        line_label(&tree.groups[group].line)
                    )
                })
                .unwrap_or_else(|| format!("{role} {id}{}", line_label(&node.line)))
        })
        .unwrap_or_else(|| id.to_string())
}

fn line_label(line: &Line) -> String {
    if let Some(active) = &line.execution {
        return if active.replacement.is_some() {
            " [replacing]".into()
        } else {
            String::new()
        };
    }
    let mut flags = Vec::new();
    if let Some(result) = &line.result {
        match result.status {
            ExecutionStatus::Stopped => {}
            ExecutionStatus::Failed => flags.push("failed"),
            ExecutionStatus::Cancelled => flags.push("cancelled"),
            ExecutionStatus::Truncated => flags.push("truncated"),
            ExecutionStatus::Interrupted => flags.push("interrupted"),
        }
    }
    if line.unread {
        flags.push("unread");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    }
}
fn agent_id(target: &Target) -> Option<SessionId> {
    match target {
        Target::Agent(id) => Some(*id),
        Target::Group(_) => None,
    }
}
fn group_id(target: &Target) -> Option<&str> {
    match target {
        Target::Group(id) => Some(id),
        Target::Agent(_) => None,
    }
}
fn wait_hint(target: &Target) -> String {
    match target {
        Target::Agent(id) => format!("wait(agent_id={id})"),
        Target::Group(id) => format!("wait(group_id={id})"),
    }
}
fn nonempty(value: &str, name: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        Err(store::error(format!("{name} cannot be empty")))
    } else {
        Ok(())
    }
}
fn encode(value: Value) -> Result<String, ToolError> {
    serde_json::to_string(&value).map_err(store::error)
}
fn truncate(text: &mut String, limit: usize) {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[truncated]");
}
