use std::{collections::VecDeque, io::Write, path::PathBuf};

use crate::input_history::InputHistory;
use anyhow::{Context, Result};
use ash_agent::{
    build_system_prompt, skill_tool, Agent, Runtime, Session, Skill, TurnHandle,
    DEFAULT_MAX_CONTEXT_TOKENS,
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

const COLLABORATION_INSTRUCTIONS: &str = "You are the main agent. Delegate only concrete, bounded work that benefits from independent execution. Reuse existing agents for related follow-ups, wait for their results, and review those results before using them. Child agents share the workspace and cannot delegate further.";

struct InteractiveController {
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
    let system_prompt = build_system_prompt(&working_dir, &skills, active_skill.as_ref())?;
    let mut tools = ash_tools::tools(
        working_dir.clone(),
        active_skill
            .as_ref()
            .and_then(|skill| skill.tools.as_deref()),
    )?;
    if !skills.is_empty() {
        tools.push(skill_tool(skills)?);
    }

    let provider = ProviderConfig {
        protocol: protocol.clone(),
        api_key: SecretString::from(api_key),
        base_url,
        model_config: load_model_config()?,
    };
    let mut agent = Agent::new(ModelId::new(model), tools)
        .with_system_prompt(system_prompt)
        .with_max_context_tokens(max_context_tokens);
    if let Some(skill) = active_skill {
        agent = skill.apply_overrides(agent);
    }
    let protocol = protocol.as_cli_name().to_string();
    let runtime = Runtime::new(create_adapter(provider));
    let base_agent = agent.clone();
    let (agent, control) = ash_collab::install_collaboration(agent, runtime.clone())?;
    let workflow = ash_workflow::tool(runtime.clone(), base_agent)?;
    let agent = agent.pushing_tools([workflow]);
    let system_prompt = [
        agent.system_prompt().unwrap_or_default(),
        COLLABORATION_INSTRUCTIONS,
        ash_workflow::WORKFLOW_INSTRUCTIONS,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let agent = agent.with_system_prompt(system_prompt);
    let subagent_events = control.events();

    Ok(AgentSetup {
        agent,
        runtime,
        protocol,
        working_dir,
        subagent_events,
    })
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
        session_id: event.session_id,
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
    let mut events = session.events();
    let turn = session.submit(input).await?;
    let mut completion = Box::pin(turn.wait());
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
                return Ok(());
            }
        }
    }
    println!();
    Ok(())
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
        SessionEvent::ToolStarted { name, .. } => {
            eprintln!("{}", format!("[tool: {name}]").cyan());
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
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UiCommand::Submit { input, reply } => {
                            let result = match self.session.try_submit(input.clone()) {
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
