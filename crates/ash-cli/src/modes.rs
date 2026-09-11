use std::{collections::VecDeque, io::Write, path::PathBuf};

use crate::input_history::InputHistory;
use anyhow::{Context, Result};
use ash_agent::{
    install_skills, Agent, Profile, PromptContext, Runtime, Session, Skill, TurnHandle,
    BASE_INSTRUCTIONS, DEFAULT_MAX_CONTEXT_TOKENS,
};
use ash_collab::{SubagentEvent, SubagentEventKind, SubagentState};
use ash_core::{Conversation, ModelId, SessionEvent, SessionId, TurnId, TurnResult};
use ash_protocol::{create_adapter, ModelConfig, Protocol, ProviderConfig};
use ash_tui::{SubagentUpdate, SubagentUpdateKind, SubagentViewState, UiCommand, UiEvent};
use futures::StreamExt;
use owo_colors::OwoColorize;
use secrecy::SecretString;
use tokio::io::AsyncBufReadExt;

use crate::{Cli, Command};

struct InteractiveController {
    control: ash_collab::AgentControl,
    session: Session,
    agent: Agent,
    runtime: Runtime,
    event_tx: tokio::sync::mpsc::Sender<UiEvent>,
    command_rx: tokio::sync::mpsc::Receiver<UiCommand>,
    subagent_events: tokio::sync::broadcast::Receiver<SubagentEvent>,
    history_store: InputHistory,
}

#[derive(Default)]
struct TurnState {
    turns: VecDeque<TurnHandle>,
}

impl TurnState {
    fn track(&mut self, turn: TurnHandle) {
        self.turns.push_back(turn);
    }

    fn finish(&mut self, id: TurnId) {
        if self.turns.front().map(TurnHandle::id) == Some(id) {
            self.turns.pop_front();
        }
    }

    fn cancel_active(&mut self) {
        if let Some(turn) = self.turns.front() {
            turn.cancellation_token().cancel();
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct AgentSetup {
    control: ash_collab::AgentControl,
    agent: Agent,
    runtime: Runtime,
    protocol: String,
    working_dir: PathBuf,
    subagent_events: tokio::sync::broadcast::Receiver<SubagentEvent>,
}

pub async fn run(mut cli: Cli) -> Result<()> {
    let command = cli.command.take();
    let setup = tokio::task::spawn_blocking(move || build_setup(&cli)).await??;
    match command {
        Some(Command::Run { prompt, .. }) => run_print(setup, prompt).await,
        None => run_interactive(setup).await,
    }
}

fn build_setup(cli: &Cli) -> Result<AgentSetup> {
    let protocol = resolve_protocol(cli)?;
    let api_key = env_value("ASH_API_KEY").context("set ASH_API_KEY in the process environment")?;
    let model = resolve_model(cli, &protocol)?;
    let base_url = cli.base_url.clone().or_else(|| env_value("ASH_BASE_URL"));
    let max_context_tokens = resolve_max_context_tokens(
        cli.max_context_tokens
            .or(env_usize("ASH_MAX_CONTEXT_TOKENS")?),
    )?;
    let working_dir = std::env::current_dir()?;
    let skills = Skill::discover(&working_dir)?;
    let active_skill = resolve_active_skill(cli, &skills)?;
    let profile = Profile::builtin(&cli.profile)?;
    let tools = ash_tools::tools(working_dir.clone(), None)?;

    let provider = ProviderConfig {
        protocol: protocol.clone(),
        api_key: SecretString::from(api_key),
        base_url,
        model_config: load_model_config()?,
    };
    let mut base = Agent::new(ModelId::new(model), tools)
        .with_system_prompt(BASE_INSTRUCTIONS)
        .with_prompt_context(PromptContext::load(&working_dir)?)
        .with_max_context_tokens(max_context_tokens);
    if let Some(skill) = &active_skill {
        base = skill.apply_overrides(base);
    }
    let definitions = build_profile_agents(&base, &skills, active_skill.as_ref())?;
    let agent = definitions
        .iter()
        .find(|agent| agent.profile() == Some(&profile))
        .cloned()
        .context("selected profile is missing from the built-in catalog")?;
    let protocol = protocol.as_cli_name().to_string();
    let runtime = Runtime::new(create_adapter(provider));
    let mut definitions = definitions
        .into_iter()
        .map(|agent| ash_collab::Definition {
            agent,
            capabilities: None,
        })
        .collect::<Vec<_>>();
    definitions.push(ash_workflow::definition(base, &skills)?);
    let control = ash_collab::AgentControl::new(runtime.clone(), definitions)?;
    let agent = control.install_root(agent)?;
    let subagent_events = control.events();

    Ok(AgentSetup {
        control,
        agent,
        runtime,
        protocol,
        working_dir,
        subagent_events,
    })
}

fn build_profile_agents(
    base: &Agent,
    skills: &[Skill],
    active_skill: Option<&Skill>,
) -> Result<Vec<Agent>> {
    Profile::builtins()?
        .into_iter()
        .map(|profile| {
            let agent = base.clone().with_profile(profile)?;
            Ok(install_skills(agent, skills.to_vec(), active_skill)?)
        })
        .collect()
}

fn subagent_update(event: SubagentEvent) -> SubagentUpdate {
    let kind = match event.kind {
        SubagentEventKind::StateChanged(state) => SubagentUpdateKind::StateChanged(match state {
            SubagentState::Idle => SubagentViewState::Idle,
            SubagentState::Running => SubagentViewState::Running,
        }),
        SubagentEventKind::Session(event) => SubagentUpdateKind::Session(event),
        SubagentEventKind::Removed => SubagentUpdateKind::Removed,
    };
    SubagentUpdate {
        root_id: event.root_id,
        session_id: event.group_id.unwrap_or(event.session_id),
        name: event.name,
        kind,
    }
}

fn resolve_protocol(cli: &Cli) -> Result<Protocol> {
    if let Some(protocol) = cli.protocol.clone() {
        return Ok(protocol);
    }
    match env_value("ASH_PROTOCOL") {
        Some(name) => name
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown protocol: {name}")),
        None => Ok(Protocol::AnthropicMessages),
    }
}

/// Resolve the model name with `--model` > `ASH_MODEL` > the protocol's
/// default model.
fn resolve_model(cli: &Cli, protocol: &Protocol) -> Result<String> {
    cli.model
        .clone()
        .or_else(|| env_value("ASH_MODEL"))
        .or_else(|| protocol.default_model().map(String::from))
        .context("set ASH_MODEL in the process environment or pass --model")
}

fn resolve_active_skill(cli: &Cli, skills: &[Skill]) -> Result<Option<Skill>> {
    cli.skill
        .as_ref()
        .map(|skill_name| {
            skills
                .iter()
                .find(|skill| skill.name == *skill_name)
                .cloned()
                .with_context(|| format!("skill not found: {skill_name}"))
        })
        .transpose()
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn load_model_config() -> Result<ModelConfig> {
    match std::env::var("ASH_MODEL_CONFIG") {
        Ok(config) => Ok(config.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(ModelConfig::default()),
        Err(error) => Err(error).context("ASH_MODEL_CONFIG must contain valid Unicode"),
    }
}

fn env_usize(name: &str) -> Result<Option<usize>> {
    env_value(name)
        .map(|value| {
            let parsed = value
                .parse::<usize>()
                .with_context(|| format!("{name} must be a positive integer"))?;
            if parsed == 0 {
                anyhow::bail!("{name} must be a positive integer");
            }
            Ok(parsed)
        })
        .transpose()
}

fn resolve_max_context_tokens(configured: Option<usize>) -> Result<usize> {
    let value = configured.unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS);
    if value == 0 {
        anyhow::bail!("maximum context tokens must be a positive integer");
    }
    Ok(value)
}

async fn run_print(setup: AgentSetup, prompt: Option<String>) -> Result<()> {
    let input = if let Some(prompt) = prompt {
        prompt
    } else {
        let mut input = String::new();
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        stdin.read_line(&mut input).await?;
        input
    };
    let session = setup.runtime.start(&setup.agent);
    let input = prepare_input(&setup.control, session.identity(), &input).await?;
    let mut events = session.events();
    let mut completion = Box::pin(session.submit(input).await?.wait());
    let mut stdout = std::io::stdout();
    let mut printed_steps = 0;

    loop {
        tokio::select! {
            biased;
            event = events.next() => match event {
                Some(Ok(event)) => print_event(event, &mut stdout, &mut printed_steps)?,
                Some(Err(error)) => tracing::warn!(%error, "session event receiver lagged"),
                None => break,
            },
            result = &mut completion => {
                let completed = result?;
                write_turn(&completed, &mut stdout, &mut printed_steps)?;
                println!();
                print_usage(&completed);
                if let Some(notice) = setup.control.collect_pending(session.identity()).await? {
                    setup.control.wait_idle(session.identity().root_id()).await;
                    let notice = setup
                        .control
                        .collect_pending(session.identity())
                        .await?
                        .unwrap_or(notice);
                    let input = prepare_input(&setup.control, session.identity(), &notice).await?;
                    completion = Box::pin(session.submit(input).await?.wait());
                    continue;
                }
                return finish_print(&setup.control, session.identity()).await;
            }
        }
    }
    println!();
    finish_print(&setup.control, session.identity()).await
}

async fn finish_print(
    control: &ash_collab::AgentControl,
    identity: ash_core::SessionIdentity,
) -> Result<()> {
    let pending = control.pending(identity).await;
    control.close(identity.id()).await;
    if let Some(notice) = ash_collab::pending_notice(&pending?) {
        anyhow::bail!("Root turn ended before collaboration was collected.\nSnapshot at turn completion:\n{notice}\nAny running descendants were cancelled during shutdown.");
    }
    Ok(())
}

async fn prepare_input(
    control: &ash_collab::AgentControl,
    identity: ash_core::SessionIdentity,
    input: &str,
) -> Result<String, ash_core::AshError> {
    let mut parts = input.trim().splitn(2, char::is_whitespace);
    if parts.next() != Some("/workflow") {
        return Ok(input.to_string());
    }
    let task = parts
        .next()
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| ash_core::AshError::Config("use /workflow <task>".into()))?;
    let receipt = control
        .create(
            ash_core::ToolContext {
                identity,
                cancellation: ash_core::CancellationToken::new(),
                deadline: None,
            },
            ash_collab::AgentArgs {
                profile: "workflow".into(),
                prompt: Some(task.into()),
            },
        )
        .await
        .map_err(|error| ash_core::AshError::Config(error.to_string()))?;
    Ok(ash_workflow::manager_input(task, &receipt))
}

/// Print the completed turn's token usage to stderr, mirroring the tool
/// diagnostics already shown there. Useful when diagnosing a run.
fn print_usage(turn: &ash_core::Turn) {
    let stats = turn.stats;
    eprintln!(
        "[usage: {} in / {} out tokens, {} tools, {} ms]",
        stats.input_tokens,
        stats.output_tokens,
        turn.completed_tool_calls(),
        stats.generation_ms,
    );
}

fn print_event(
    event: SessionEvent,
    output: &mut impl Write,
    printed_steps: &mut usize,
) -> std::io::Result<()> {
    match event {
        SessionEvent::StepCommitted { step, index, .. } if index == *printed_steps => {
            write_step(&step, output)?;
            *printed_steps += 1;
        }
        SessionEvent::ToolStarted {
            name, arguments, ..
        } => {
            let target = if name == "wait" {
                ["agent_id", "group_id"]
                    .into_iter()
                    .find_map(|key| {
                        arguments
                            .get(key)
                            .and_then(|value| value.as_str())
                            .map(|id| format!(" waiting for {key}={id}"))
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            eprintln!("{}", format!("[tool: {name}]{target}").cyan());
        }
        SessionEvent::ToolFinished {
            result: Err(error), ..
        } => eprintln!("{}", format!("[error: {error}]").red()),
        SessionEvent::Finished { turn, .. } => {
            write_turn(&turn, output, printed_steps)?;
            match &turn.result {
                TurnResult::Failed(error) => {
                    eprintln!("{}", format!("[error: {error}]").red());
                }
                TurnResult::Stopped(_) | TurnResult::Cancelled | TurnResult::Truncated => {}
            }
        }
        SessionEvent::Discarded {
            error: Some(error), ..
        } => eprintln!("{}", format!("[error: {error}]").red()),
        _ => {}
    }
    Ok(())
}

fn write_step(step: &ash_core::Step, output: &mut impl Write) -> std::io::Result<()> {
    for text in step.items.iter().filter_map(ash_core::Item::text) {
        output.write_all(text.as_bytes())?;
    }
    output.flush()
}

fn write_turn(
    turn: &ash_core::Turn,
    output: &mut impl Write,
    printed_steps: &mut usize,
) -> std::io::Result<()> {
    for step in turn.steps.iter().skip(*printed_steps) {
        write_step(step, output)?;
        *printed_steps += 1;
    }
    Ok(())
}

async fn run_interactive(setup: AgentSetup) -> Result<()> {
    let AgentSetup {
        control,
        agent,
        runtime,
        protocol,
        working_dir,
        subagent_events,
    } = setup;
    let model = agent.model().as_str().to_string();
    let (command_tx, command_rx) = tokio::sync::mpsc::channel::<UiCommand>(16);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    let event_rx = tokio_stream::wrappers::ReceiverStream::new(event_rx);
    let history_store = InputHistory::default();
    let input_history = match history_store.load().await {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(%error, "failed to load input history");
            Vec::new()
        }
    };
    let app = ash_tui::App::new(protocol, model, working_dir).with_input_history(input_history);
    let app_handle = tokio::spawn(async move { app.run(event_rx, command_tx).await });
    let session = runtime.start(&agent);

    InteractiveController {
        control,
        session,
        agent,
        runtime,
        event_tx,
        command_rx,
        subagent_events,
        history_store,
    }
    .run()
    .await;

    app_handle.await??;
    Ok(())
}

impl InteractiveController {
    async fn run(mut self) {
        let _ = self
            .event_tx
            .send(UiEvent::ConversationChanged {
                session_id: self.session.id(),
                conversation: Conversation::new(),
                input: None,
            })
            .await;
        let mut events = self.session.events();
        let mut turns = TurnState::default();
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        let finished = matches!(&event, SessionEvent::Finished { .. });
                        match &event {
                            SessionEvent::Finished { turn, .. } => turns.finish(turn.id),
                            SessionEvent::Discarded { turn_id, .. } => turns.finish(*turn_id),
                            SessionEvent::Started(_)
                            | SessionEvent::Retrying { .. }
                            | SessionEvent::Text { .. }
                            | SessionEvent::Thought { .. }
                            | SessionEvent::Activity { .. }
                            | SessionEvent::Context { .. }
                            | SessionEvent::ToolStarted { .. }
                            | SessionEvent::ToolFinished { .. }
                            | SessionEvent::StepCommitted { .. } => {}
                        }
                        let _ = self.event_tx.send(UiEvent::Session(event)).await;
                        if finished {
                            match self.control.pending(self.session.identity()).await {
                                Ok(pending) => {
                                    if let Some(notice) = ash_collab::pending_notice(&pending) {
                                        self.send_ui_event(UiEvent::Warning(notice)).await;
                                    }
                                }
                                Err(error) => self.send_ui_event(UiEvent::Warning(format!("Cannot inspect collaboration: {error}"))).await,
                            }
                        }
                    }
                    Some(Err(error)) => tracing::warn!(%error, "session event receiver lagged"),
                    None => break,
                },
                event = self.subagent_events.recv() => match event {
                    Ok(event) => {
                        let _ = self
                            .event_tx
                            .send(UiEvent::Subagent(subagent_update(event)))
                            .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "subagent event receiver lagged");
                        self.refresh_subagents().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UiCommand::Submit { input, reply } => {
                            let prepared = prepare_input(&self.control, self.session.identity(), &input).await;
                            let result = match prepared.and_then(|input| self.session.try_submit(input)) {
                                Ok(turn) => {
                                    let turn_id = turn.id();
                                    turns.track(turn);
                                    self.record_input(&input).await;
                                    Ok(turn_id)
                                }
                                Err(error) => Err(error.into()),
                            };
                            let _ = reply.send(result);
                        }
                        UiCommand::Cancel => {
                            turns.cancel_active();
                        }
                        UiCommand::Undo => {
                            if self.undo().await {
                                events = self.session.events();
                                turns.reset();
                            }
                        }
                        UiCommand::Compact => self.compact_session().await,
                        UiCommand::NewSession => {
                            self.control.close(self.session.id()).await;
                            self.session = self.runtime.start(&self.agent);
                            let _ = self
                                .event_tx
                                .send(UiEvent::ConversationChanged {
                                    session_id: self.session.id(),
                                    conversation: Conversation::new(),
                                    input: None,
                                })
                                .await;
                            events = self.session.events();
                            turns.reset();
                        }
                        UiCommand::ListSessions => self.list_sessions().await,
                        UiCommand::ResumeSession(session_id) => {
                            self.resume_session(session_id).await;
                            events = self.session.events();
                            turns.reset();
                        }
                        UiCommand::ForkSession(turn_id) => {
                            self.fork_session(turn_id).await;
                            events = self.session.events();
                            turns.reset();
                        }
                        UiCommand::Exit => {
                            turns.cancel_active();
                            self.control.close(self.session.id()).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn record_input(&self, input: &str) {
        if let Err(error) = self.history_store.append(input).await {
            tracing::warn!(%error, "failed to persist input history");
        }
    }

    async fn refresh_subagents(&self) {
        match self.control.snapshots(self.session.id()).await {
            Ok(events) => {
                self.send_ui_event(UiEvent::SubagentsChanged {
                    root_id: self.session.id(),
                    agents: events.into_iter().map(subagent_update).collect(),
                })
                .await
            }
            Err(error) => {
                self.send_ui_event(UiEvent::CommandFailed(format!(
                    "Cannot refresh collaboration: {error}"
                )))
                .await
            }
        }
    }

    async fn send_ui_event(&self, event: UiEvent) {
        let _ = self.event_tx.send(event).await;
    }

    async fn undo(&mut self) -> bool {
        let event = match self.session.undo().await {
            Ok(Some(forked)) => {
                let conversation = match forked.session.conversation().await {
                    Ok(conversation) => conversation,
                    Err(error) => {
                        self.send_ui_event(UiEvent::CommandFailed(format!(
                            "Failed to restore the undo result: {error}"
                        )))
                        .await;
                        return false;
                    }
                };
                let session_id = forked.session.id();
                self.control.close(self.session.id()).await;
                self.session = forked.session;
                UiEvent::ConversationChanged {
                    session_id,
                    conversation,
                    input: Some(forked.input),
                }
            }
            Ok(None) => {
                UiEvent::CommandFailed("No submitted turn is available to undo.".to_string())
            }
            Err(error) => UiEvent::CommandFailed(format!("Failed to undo the last turn: {error}")),
        };
        let changed = matches!(event, UiEvent::ConversationChanged { .. });
        self.send_ui_event(event).await;
        changed
    }

    async fn compact_session(&self) {
        let result = tokio::select! {
            biased;
            () = self.event_tx.closed() => return,
            result = self.session.compact() => result,
        };
        let event = match result {
            Ok(changed) => match self.session.conversation().await {
                Ok(conversation) => UiEvent::CompactionCompleted {
                    changed,
                    conversation,
                },
                Err(error) => {
                    UiEvent::CommandFailed(format!("Failed to load compacted context: {error}"))
                }
            },
            Err(error) => UiEvent::CommandFailed(format!("Failed to compact context: {error}")),
        };
        self.send_ui_event(event).await;
    }

    async fn list_sessions(&self) {
        let event = match self.runtime.list_sessions(Some(self.session.id())).await {
            Ok(sessions) => UiEvent::SessionsListed { sessions },
            Err(error) => UiEvent::CommandFailed(format!("Failed to list saved chats: {error}")),
        };
        self.send_ui_event(event).await;
    }

    async fn resume_session(&mut self, session_id: SessionId) {
        let event = match self.runtime.resume(&self.agent, session_id).await {
            Ok(Some(session)) => match session.conversation().await {
                Ok(conversation) => {
                    if let Err(error) = self.control.restore(session.identity()).await {
                        self.send_ui_event(UiEvent::CommandFailed(format!(
                            "Failed to restore collaboration: {error}"
                        )))
                        .await;
                        return;
                    }
                    self.control.close(self.session.id()).await;
                    let session_id = session.id();
                    self.session = session;
                    UiEvent::ConversationChanged {
                        session_id,
                        conversation,
                        input: None,
                    }
                }
                Err(error) => UiEvent::CommandFailed(format!("Failed to restore chat: {error}")),
            },
            Ok(None) => {
                UiEvent::CommandFailed("That saved chat is no longer available.".to_string())
            }
            Err(error) => UiEvent::CommandFailed(format!("Failed to resume saved chat: {error}")),
        };
        self.send_ui_event(event).await;
    }

    async fn fork_session(&mut self, turn_id: TurnId) {
        let event = match self.session.fork_at(turn_id).await {
            Ok(Some(forked)) => {
                let session = forked.session;
                match session.conversation().await {
                    Ok(conversation) => {
                        self.control.close(self.session.id()).await;
                        let session_id = session.id();
                        self.session = session;
                        UiEvent::ConversationChanged {
                            session_id,
                            conversation,
                            input: Some(forked.input),
                        }
                    }
                    Err(error) => {
                        UiEvent::CommandFailed(format!("Failed to fork the current chat: {error}"))
                    }
                }
            }
            Ok(None) => UiEvent::CommandFailed(
                "That prompt is no longer available to fork from.".to_string(),
            ),
            Err(error) => {
                UiEvent::CommandFailed(format!("Failed to fork the current chat: {error}"))
            }
        };
        self.send_ui_event(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print_test_control(
        running: bool,
    ) -> (
        tempfile::TempDir,
        ash_collab::AgentControl,
        ash_core::SessionIdentity,
    ) {
        struct Model(bool);
        impl ash_core::ModelClient for Model {
            fn stream(
                &self,
                _: ash_core::ModelRequest,
            ) -> Result<ash_core::ModelStream, ash_core::ProtocolError> {
                if self.0 {
                    Ok(Box::pin(futures::stream::pending()))
                } else {
                    Ok(Box::pin(futures::stream::iter([
                        Ok(ash_core::ModelEvent::Text("child finished".into())),
                        Ok(ash_core::ModelEvent::Stop(ash_core::StopReason::EndTurn)),
                    ])))
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(std::sync::Arc::new(Model(running)))
            .with_session_directory(directory.path());
        let definition = ash_collab::Definition {
            agent: Agent::new(ModelId::new("test"), Vec::new())
                .with_profile(Profile::builtin("default").unwrap())
                .unwrap(),
            capabilities: None,
        };
        let control = ash_collab::AgentControl::new(runtime, vec![definition]).unwrap();
        (
            directory,
            control,
            ash_core::SessionIdentity::root(SessionId::new()),
        )
    }

    #[tokio::test]
    async fn print_exit_requires_receipt_and_always_closes_running_descendants() {
        for state in ["running", "unread"] {
            let (_directory, control, root) = print_test_control(state == "running");
            let context = ash_core::ToolContext {
                identity: root,
                cancellation: ash_core::CancellationToken::new(),
                deadline: None,
            };
            let created: serde_json::Value = serde_json::from_str(
                &control
                    .create(
                        context.clone(),
                        ash_collab::AgentArgs {
                            profile: "default".into(),
                            prompt: Some("do work".into()),
                        },
                    )
                    .await
                    .unwrap(),
            )
            .unwrap();
            let child = serde_json::from_value(created["agent_id"].clone()).unwrap();
            let args = ash_collab::WaitArgs {
                agent_id: Some(child),
                group_id: None,
            };
            if state != "running" {
                control.wait(context.clone(), args.clone()).await.unwrap();
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                finish_print(&control, root),
            )
            .await
            .unwrap();
            let error = result.unwrap_err().to_string();
            assert!(error.contains(&child.to_string()));
            assert!(error.contains(state));
            assert!(error.contains("before collaboration was collected"));
            assert!(control
                .pending(root)
                .await
                .unwrap()
                .iter()
                .all(|work| work.state != ash_collab::PendingState::Running));
        }
    }

    #[tokio::test]
    async fn print_exit_without_delegation_does_not_require_wait() {
        let (_directory, control, root) = print_test_control(false);
        assert!(finish_print(&control, root).await.is_ok());
    }

    #[tokio::test]
    async fn workflow_shortcut_creates_one_ordinary_profiled_manager() {
        struct Model;
        impl ash_core::ModelClient for Model {
            fn stream(
                &self,
                request: ash_core::ModelRequest,
            ) -> Result<ash_core::ModelStream, ash_core::ProtocolError> {
                assert!(request.tools.iter().any(|tool| tool.name == "workflow"));
                Ok(Box::pin(futures::stream::iter([
                    Ok(ash_core::ModelEvent::Text("manager finished".into())),
                    Ok(ash_core::ModelEvent::Stop(ash_core::StopReason::EndTurn)),
                ])))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let runtime =
            Runtime::new(std::sync::Arc::new(Model)).with_session_directory(directory.path());
        let base = Agent::new(
            ModelId::new("test"),
            ash_tools::tools(directory.path(), None).unwrap(),
        );
        let definitions = vec![
            ash_collab::Definition {
                agent: base
                    .clone()
                    .with_profile(Profile::builtin("default").unwrap())
                    .unwrap(),
                capabilities: None,
            },
            ash_workflow::definition(base, &[]).unwrap(),
        ];
        let control = ash_collab::AgentControl::new(runtime, definitions).unwrap();
        let root = ash_core::SessionIdentity::root(SessionId::new());
        assert_eq!(
            prepare_input(&control, root, "normal task").await.unwrap(),
            "normal task"
        );
        assert!(prepare_input(&control, root, "/workflow").await.is_err());
        let prepared = prepare_input(&control, root, "/workflow inspect and verify")
            .await
            .unwrap();
        assert!(prepared.contains("inspect and verify"));
        assert!(prepared.contains("Do not create another manager"));
        let context = ash_core::ToolContext {
            identity: root,
            cancellation: ash_core::CancellationToken::new(),
            deadline: None,
        };
        let listing: serde_json::Value = serde_json::from_str(
            &control
                .list(context.clone(), ash_collab::ListArgs { group_id: None })
                .await
                .unwrap(),
        )
        .unwrap();
        let agents = listing["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["profile"], "workflow");
        assert_eq!(agents[0]["parent_id"], serde_json::json!(root.id()));
        let agent_id: SessionId = serde_json::from_value(agents[0]["agent_id"].clone()).unwrap();
        let result = control
            .wait(
                context,
                ash_collab::WaitArgs {
                    agent_id: Some(agent_id),
                    group_id: None,
                },
            )
            .await
            .unwrap();
        assert!(result.contains("manager finished"));
        control.close(root.id()).await;
    }

    #[test]
    fn builtin_profiles_select_tools_without_inheriting_management() {
        let directory = tempfile::tempdir().unwrap();
        let tools = ash_tools::tools(directory.path().to_path_buf(), None).unwrap();
        let base = Agent::new(ash_core::ModelId::new("model"), tools);
        let agents = build_profile_agents(&base, &[], None).unwrap();
        for agent in agents {
            let profile = agent.profile().unwrap();
            let names = agent
                .tools()
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>();
            assert_eq!(names.contains(&"write"), profile.name() == "default");
            assert_eq!(names.contains(&"edit"), profile.name() == "default");
            assert!(names.contains(&"bash"));
            assert!(!names.contains(&"agent"));
            assert!(!names.contains(&"workflow"));
            assert!(agent
                .system_prompt()
                .unwrap()
                .starts_with(profile.instructions()));
            assert!(agent
                .system_prompt()
                .unwrap()
                .contains("Shell scripts are not automatically analyzed"));
        }
    }

    #[test]
    fn startup_skill_adds_tools_to_profiles_without_removing_existing_tools() {
        let directory = tempfile::tempdir().unwrap();
        let skill_dir = directory.path().join(".agents/skills/ash-test-extension");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"),
            "---\nname: ash-test-extension\ndescription: Test extension\ntools: [write, read]\n---\nextension rules").unwrap();
        let skill = Skill::discover(directory.path())
            .unwrap()
            .into_iter()
            .find(|skill| skill.name == "ash-test-extension")
            .unwrap();
        let base = Agent::new(
            ash_core::ModelId::new("model"),
            ash_tools::tools(directory.path().to_path_buf(), None).unwrap(),
        );
        let agents =
            build_profile_agents(&base, std::slice::from_ref(&skill), Some(&skill)).unwrap();
        for agent in agents {
            let names = agent
                .tools()
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>();
            assert!(names.contains(&"write"));
            assert!(names.contains(&"bash"));
            assert!(names.contains(&"skill"));
            assert_eq!(names.iter().filter(|name| **name == "read").count(), 1);
            let prompt = agent.system_prompt().unwrap();
            assert_eq!(prompt.matches("extension rules").count(), 1);
            assert!(!agent
                .without_tools(&["skill"])
                .system_prompt()
                .unwrap()
                .contains("extension rules"));
        }
        let mut invalid = skill.clone();
        invalid.tools = Some(vec!["unknown".into()]);
        assert!(build_profile_agents(&base, &[], Some(&invalid)).is_err());
        assert_eq!(base.tools().len(), 7);
    }

    #[test]
    fn print_output_ignores_retry_previews_and_recovers_missed_steps_once() {
        use ash_core::{Input, Item, Step, StopReason, Turn};
        use std::sync::Arc;
        let id = TurnId::new();
        let step = Arc::new(Step {
            items: vec![Item::Text("final".into())],
        });
        let mut output = Vec::new();
        let mut printed = 0;
        print_event(
            SessionEvent::Text {
                turn_id: id,
                text: "discarded".into(),
            },
            &mut output,
            &mut printed,
        )
        .unwrap();
        print_event(
            SessionEvent::Retrying { turn_id: id },
            &mut output,
            &mut printed,
        )
        .unwrap();
        assert!(output.is_empty());
        print_event(
            SessionEvent::StepCommitted {
                turn_id: id,
                index: 0,
                step: Arc::clone(&step),
            },
            &mut output,
            &mut printed,
        )
        .unwrap();
        let turn = Arc::new(Turn {
            id,
            input: Input::user("question"),
            steps: vec![
                step,
                Arc::new(Step {
                    items: vec![Item::Text(" tail".into())],
                }),
            ],
            result: TurnResult::Stopped(StopReason::EndTurn),
            stats: Default::default(),
        });
        print_event(
            SessionEvent::Finished {
                turn: Arc::clone(&turn),
                summary: None,
            },
            &mut output,
            &mut printed,
        )
        .unwrap();
        write_turn(&turn, &mut output, &mut printed).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "final tail");
        assert_eq!(printed, 2);
    }

    #[test]
    fn model_context_limit_defaults_to_one_million() {
        // DeepSeek-V4 serves a 1M-token context window; that is the default
        // budget we reserve for the model context projection.
        assert_eq!(resolve_max_context_tokens(None).unwrap(), 1_000_000);
        assert_eq!(resolve_max_context_tokens(Some(64_000)).unwrap(), 64_000);
        assert!(resolve_max_context_tokens(Some(0)).is_err());
    }

    #[test]
    fn protocol_names_have_one_parser() {
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::Completions,
            Protocol::Responses,
        ] {
            let name = protocol.as_cli_name();
            assert_eq!(name.parse::<Protocol>().unwrap().as_cli_name(), name);
        }
        assert!("responses".parse::<Protocol>().is_err());
    }
}
