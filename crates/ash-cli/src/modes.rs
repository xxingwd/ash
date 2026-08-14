use std::time::Duration;

use crate::message_history::MessageHistoryStore;
use anyhow::{Context, Result};
use ash_agent::{
    build_system_prompt, skill_tool, Agent, Runtime, Session, SessionOptions, Skill, Turn,
    DEFAULT_MAX_CONTEXT_TOKENS,
};
use ash_collab::SubagentSnapshot;
use ash_core::{
    EventKind, LiveEvent, MessageId, ModelId, SessionId, SessionView, TurnId, TurnResult,
};
use ash_protocol::{create_adapter, Protocol, ProviderConfig};
use ash_tui::{UiCommand, UiError};
use futures::StreamExt;
use owo_colors::OwoColorize;
use secrecy::SecretString;
use tokio::io::AsyncBufReadExt;

use crate::{Cli, Command};

const DEFAULT_MAX_TURNS: u32 = 100;
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_mins(2);

struct InteractiveController {
    session: Session,
    agent: Agent,
    options: SessionOptions,
    runtime: Runtime,
    event_tx: tokio::sync::mpsc::Sender<EventKind>,
    command_rx: tokio::sync::mpsc::Receiver<UiCommand>,
    history_store: MessageHistoryStore,
}

#[derive(Default)]
struct TurnState {
    active: Option<Turn>,
}

impl TurnState {
    fn track(&mut self, turn: Turn) {
        self.active = Some(turn);
    }

    fn finish(&mut self, id: Option<TurnId>) {
        if self.active.as_ref().map(Turn::id) == id {
            self.active = None;
        }
    }

    fn cancel_active(&self) {
        if let Some(turn) = &self.active {
            turn.cancellation_token().cancel();
        }
    }

    async fn steer_active(&self, input: String) -> Result<(), UiError> {
        let turn = self.active.as_ref().ok_or_else(|| {
            UiError::Agent(ash_core::AshError::Session(
                ash_core::SessionError::InactiveTurn,
            ))
        })?;
        turn.steer(input).await.map_err(UiError::from)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct AgentSetup {
    agent: Agent,
    options: SessionOptions,
    runtime: Runtime,
    subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
}

pub async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Some(Command::Run { prompt, .. }) => {
            let setup = build_config(&cli)?;
            run_print(setup, prompt.clone()).await
        }
        None => {
            let setup = build_config(&cli)?;
            run_interactive(setup).await
        }
    }
}

fn build_config(cli: &Cli) -> Result<AgentSetup> {
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
    tools.push(skill_tool(skills)?);

    let provider = ProviderConfig {
        protocol: protocol.clone(),
        api_key: SecretString::from(api_key),
        base_url,
    };
    let mut agent = Agent::new(ModelId::new(model), tools)
        .with_system_prompt(system_prompt)
        .with_max_turns(DEFAULT_MAX_TURNS)
        .with_max_context_tokens(max_context_tokens);
    let options = SessionOptions {
        working_dir,
        tool_timeout: DEFAULT_TOOL_TIMEOUT,
        ..SessionOptions::default()
    };
    if let Some(skill) = active_skill {
        agent = skill.apply_overrides(agent);
    }
    let runtime = Runtime::new(create_adapter(provider), protocol.as_cli_name());
    let max_concurrent_agents = env_usize("ASH_MAX_CONCURRENT_AGENTS")?;
    let (agent, control) = ash_collab::install_subagent_tools(
        agent,
        options.clone(),
        runtime.clone(),
        max_concurrent_agents,
    )?;
    let subagent_monitor = control.map(|control| control.subscribe());

    Ok(AgentSetup {
        agent,
        options,
        runtime,
        subagent_monitor,
    })
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
    let session = setup.runtime.start(&setup.agent, &setup.options);
    let mut events = session.events();
    let turn = session.submit(input).await?;
    let mut completion = Box::pin(turn.wait());

    loop {
        tokio::select! {
            biased;
            event = events.next() => match event {
                Some(Ok(event)) => print_event(event.kind),
                Some(Err(error)) => tracing::warn!(%error, "agent event receiver lagged"),
                None => break,
            },
            result = &mut completion => {
                result?;
                break;
            }
        }
    }
    println!();
    print_usage(&session).await;
    Ok(())
}

/// Print the completed turn's token usage to stderr, mirroring the tool
/// diagnostics already shown there. Useful when diagnosing a run.
async fn print_usage(session: &Session) {
    let Ok(view) = session.view().await else {
        return;
    };
    let Some(turn) = view.turns.last() else {
        return;
    };
    let Some(usage) = turn.usage else {
        return;
    };
    eprintln!(
        "[usage: {} in / {} out tokens, {} ms{}]",
        usage.input_tokens,
        usage.output_tokens,
        usage.generation_ms,
        if usage.estimated { " (estimated)" } else { "" }
    );
}

fn print_event(event: EventKind) {
    match event {
        EventKind::Live(LiveEvent::TextDelta(text)) => print!("{text}"),
        EventKind::Live(LiveEvent::ToolStarted { name, .. }) => {
            eprintln!("{}", format!("[tool: {name}]").cyan());
        }
        EventKind::Live(LiveEvent::ToolFinished {
            is_error: true,
            output,
            ..
        }) => eprintln!("{}", format!("[error: {output}]").red()),
        EventKind::Turn(view) => match view.result {
            TurnResult::Failed(error) | TurnResult::Interrupted(error) => {
                eprintln!("{}", format!("[error: {error}]").red());
            }
            TurnResult::Completed(_) => {}
        },
        EventKind::Error(error) => eprintln!("{}", format!("[error: {error}]").red()),
        _ => {}
    }
}

async fn run_interactive(setup: AgentSetup) -> Result<()> {
    let AgentSetup {
        agent,
        options,
        runtime,
        subagent_monitor,
    } = setup;
    let protocol = runtime.model_backend().to_string();
    let model = agent.model().as_str().to_string();
    let working_dir = options.working_dir.clone();
    let context_limit = Some(u64::try_from(agent.max_context_tokens()).unwrap_or(u64::MAX));
    let (command_tx, command_rx) = tokio::sync::mpsc::channel::<UiCommand>(16);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    let event_rx = tokio_stream::wrappers::ReceiverStream::new(event_rx);
    let history_store = MessageHistoryStore::default();
    let input_history = match history_store.load().await {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(%error, "failed to load input history");
            Vec::new()
        }
    };
    let app = ash_tui::App::new(protocol, model, working_dir)
        .with_context_limit(context_limit)
        .with_input_history(input_history)
        .with_subagent_monitor(subagent_monitor);
    let app_handle = tokio::spawn(async move { app.run(event_rx, command_tx).await });
    let session = runtime.start(&agent, &options);

    InteractiveController {
        session,
        agent,
        options,
        runtime,
        event_tx,
        command_rx,
        history_store,
    }
    .run()
    .await;

    app_handle.await??;
    Ok(())
}

impl InteractiveController {
    async fn run(mut self) {
        let mut events = self.session.events();
        let mut turns = TurnState::default();
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        if matches!(event.kind, EventKind::Turn(_)) {
                            turns.finish(event.turn_id);
                        }
                        let _ = self.event_tx.send(event.kind).await;
                    }
                    Some(Err(error)) => tracing::warn!(%error, "agent event receiver lagged"),
                    None => break,
                },
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UiCommand::Submit { input, reply } => {
                            let result = match self.session.submit(input.clone()).await {
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
                        UiCommand::Steer { input, reply } => {
                            let result = turns.steer_active(input.clone()).await;
                            if result.is_ok() {
                                self.record_input(&input).await;
                            }
                            let _ = reply.send(result);
                        }
                        UiCommand::Cancel => {
                            turns.cancel_active();
                        }
                        UiCommand::Rollback => self.rollback_last_turn().await,
                        UiCommand::Compact => self.compact_session().await,
                        UiCommand::NewSession => {
                            self.session = self.runtime.start(&self.agent, &self.options);
                            events = self.session.events();
                            turns.reset();
                        }
                        UiCommand::ListSessions => self.list_sessions().await,
                        UiCommand::ListForkPoints => self.list_fork_points().await,
                        UiCommand::ResumeSession(session_id) => {
                            self.resume_session(session_id).await;
                            events = self.session.events();
                            turns.reset();
                        }
                        UiCommand::ForkSession(message_id) => {
                            self.fork_session(message_id).await;
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
        if let Err(error) = self.history_store.append(self.session.id(), input).await {
            tracing::warn!(%error, "failed to persist input history");
        }
    }

    async fn rollback_last_turn(&self) {
        let event = match self.session.rollback().await {
            Ok(Some(prompt)) => {
                if let Err(error) = self.history_store.undo(self.session.id(), &prompt).await {
                    tracing::warn!(%error, "failed to undo input history entry");
                }
                EventKind::TurnRolledBack { prompt }
            }
            Ok(None) => EventKind::Error("No submitted turn is available to undo.".to_string()),
            Err(error) => EventKind::Error(format!("Failed to undo the last turn: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn compact_session(&self) {
        let event = match self.session.compact().await {
            Ok(result) => EventKind::Compacted {
                before: u64::try_from(result.before_tokens).unwrap_or(u64::MAX),
                after: u64::try_from(result.after_tokens).unwrap_or(u64::MAX),
                dropped: u64::try_from(result.dropped_messages).unwrap_or(u64::MAX),
                automatic: false,
            },
            Err(error) => EventKind::Error(format!("Failed to compact context: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn list_sessions(&self) {
        let event = match self.runtime.sessions(Some(self.session.id())).await {
            Ok(sessions) => EventKind::SessionsListed { sessions },
            Err(error) => EventKind::Error(format!("Failed to list saved chats: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn list_fork_points(&self) {
        let event = match self.session.fork_points().await {
            Ok(points) => EventKind::ForkPointsListed { points },
            Err(error) => EventKind::Error(format!("Failed to list fork points: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    /// Estimate the model-context size for a given message history, using the
    /// same estimator the runtime uses when the API does not report usage.
    async fn resume_session(&mut self, session_id: SessionId) {
        let event = match self
            .runtime
            .resume(&self.agent, &self.options, session_id)
            .await
        {
            Ok(Some(session)) => {
                self.session = session;
                self.with_current_view("restore chat", |view| EventKind::Restored { view })
                    .await
            }
            Ok(None) => EventKind::Error("That saved chat is no longer available.".to_string()),
            Err(error) => EventKind::Error(format!("Failed to resume saved chat: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn fork_session(&mut self, message_id: MessageId) {
        let event = match self.session.fork_at(message_id).await {
            Ok(Some(forked)) => {
                let prompt = forked.prompt;
                self.session = forked.session;
                self.with_current_view("fork the current chat", |view| EventKind::SessionForked {
                    view,
                    prompt,
                })
                .await
            }
            Ok(None) => {
                EventKind::Error("That prompt is no longer available to fork from.".to_string())
            }
            Err(error) => EventKind::Error(format!("Failed to fork the current chat: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    /// Read the session's current view and map it to an event. Centralizes the
    /// error event produced when the view cannot be read.
    async fn with_current_view(
        &self,
        view_error_context: &str,
        on_view: impl FnOnce(SessionView) -> EventKind,
    ) -> EventKind {
        match self.session.view().await {
            Ok(view) => on_view(view),
            Err(error) => EventKind::Error(format!("Failed to {view_error_context}: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
