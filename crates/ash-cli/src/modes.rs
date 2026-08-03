use anyhow::{Context, Result};
use ash_agent::{
    build_system_prompt, estimate_request_tokens, skill_tool, Agent, MessageHistoryStore, Runtime,
    Skill, Thread, ThreadOptions, DEFAULT_MAX_CONTEXT_TOKENS,
};
use ash_core::{
    CancellationToken, EventKind, MessageId, ModelId, SubagentSnapshot, ThreadId, TurnId,
};
use ash_protocol::{create_adapter, Protocol, ProviderConfig};
use ash_tui::UiCommand;
use futures::StreamExt;
use owo_colors::OwoColorize;
use secrecy::SecretString;

use crate::Cli;

struct InteractiveController {
    thread: Thread,
    agent: Agent,
    options: ThreadOptions,
    runtime: Runtime,
    event_tx: tokio::sync::mpsc::Sender<EventKind>,
    command_rx: tokio::sync::mpsc::Receiver<UiCommand>,
    history_store: MessageHistoryStore,
}

struct AgentSetup {
    agent: Agent,
    options: ThreadOptions,
    runtime: Runtime,
    subagent_monitor: Option<tokio::sync::watch::Receiver<Vec<SubagentSnapshot>>>,
}

pub async fn run(cli: Cli) -> Result<()> {
    let setup = build_config(&cli)?;
    if cli.print {
        run_print(setup, cli.prompt).await
    } else {
        run_interactive(setup).await
    }
}

fn build_config(cli: &Cli) -> Result<AgentSetup> {
    let protocol_name = cli
        .protocol
        .clone()
        .or_else(|| env_value("ASH_PROTOCOL"))
        .unwrap_or_else(|| "anthropic".into());
    let protocol = parse_protocol(&protocol_name)?;
    let api_key = env_value("ASH_API_KEY").context("set ASH_API_KEY in the process environment")?;
    let model = cli.model.clone().or_else(|| env_value("ASH_MODEL"));
    let model = match model {
        Some(model) => model,
        None if matches!(&protocol, Protocol::AnthropicMessages) => {
            "claude-sonnet-4-20250514".into()
        }
        None => anyhow::bail!("set ASH_MODEL in the process environment or pass --model"),
    };
    let base_url = cli.base_url.clone().or_else(|| env_value("ASH_BASE_URL"));
    let configured_max_context_tokens = match cli.max_context_tokens {
        Some(value) => Some(value),
        None => env_usize("ASH_MAX_CONTEXT_TOKENS")?,
    };
    let max_context_tokens = resolve_max_context_tokens(configured_max_context_tokens)?;
    let working_dir = std::env::current_dir()?;
    let skills = Skill::discover(&working_dir)?;
    let active_skill = cli
        .skill
        .as_ref()
        .map(|skill_name| {
            skills
                .iter()
                .find(|skill| skill.name == *skill_name)
                .cloned()
                .with_context(|| format!("skill not found: {skill_name}"))
        })
        .transpose()?;
    let system_prompt = build_system_prompt(&working_dir, &skills, active_skill.as_ref())?;
    let mut tools = ash_tools::tools(
        working_dir.clone(),
        active_skill
            .as_ref()
            .and_then(|skill| skill.tools.as_deref()),
    )?;
    tools.push(skill_tool(skills.clone()));

    let provider = ProviderConfig {
        protocol: protocol.clone(),
        api_key: SecretString::from(api_key),
        base_url,
    };
    let mut agent = Agent {
        system_prompt: Some(system_prompt),
        model: ModelId::new(model),
        tools,
        max_turns: 100,
        max_context_tokens,
        context_policy: std::sync::Arc::new(ash_agent::DefaultContextPolicy),
    };
    let options = ThreadOptions {
        working_dir,
        tool_timeout: std::time::Duration::from_secs(120),
        ..ThreadOptions::default()
    };
    if let Some(skill) = active_skill {
        skill.apply_overrides(&mut agent);
    }
    let runtime = Runtime::new(create_adapter(provider), protocol.as_cli_name());
    let max_concurrent_agents = env_usize("ASH_MAX_CONCURRENT_AGENTS")?;
    let subagent_monitor = ash_collab::install_subagent_tools(
        &mut agent,
        options.clone(),
        runtime.clone(),
        max_concurrent_agents,
    )
    .map(|control| control.subscribe());

    Ok(AgentSetup {
        agent,
        options,
        runtime,
        subagent_monitor,
    })
}

fn parse_protocol(name: &str) -> Result<Protocol> {
    name.parse()
        .map_err(|_| anyhow::anyhow!("unknown protocol: {name}"))
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
    let input = match prompt {
        Some(prompt) => prompt,
        None => {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input
        }
    };
    let thread = setup.runtime.start(setup.agent, setup.options);
    let mut events = thread.events();
    let turn = thread.submit(input).await?;
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
    Ok(())
}

fn print_event(event: EventKind) {
    match event {
        EventKind::TextDelta(text) => print!("{text}"),
        EventKind::ToolCallStart { name, .. } => {
            eprintln!("{}", format!("[tool: {name}]").cyan());
        }
        EventKind::ToolCallEnd {
            is_error: true,
            output,
            ..
        } => eprintln!("{}", format!("[error: {output}]").red()),
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
    let model = agent.model.as_str().to_string();
    let working_dir = options.working_dir.clone();
    let context_limit = Some(u64::try_from(agent.max_context_tokens).unwrap_or(u64::MAX));
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
    let thread = runtime.start(agent.clone(), options.clone());

    InteractiveController {
        thread,
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
        let mut events = self.thread.events();
        let mut turns = std::collections::HashMap::<TurnId, CancellationToken>::new();
        let mut active = None;
        let mut rollback_after_cancel = false;
        loop {
            tokio::select! {
                biased;
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        if matches!(event.kind, EventKind::TurnStarted) {
                            active = event.turn_id;
                        }
                        let completed = matches!(event.kind, EventKind::TurnCompleted { .. });
                        let completed_turn = event.turn_id;
                        let _ = self.event_tx.send(event.kind).await;
                        if completed {
                            if let Some(turn_id) = completed_turn {
                                turns.remove(&turn_id);
                            }
                            active = None;
                            if rollback_after_cancel {
                                rollback_after_cancel = false;
                                self.rollback_last_turn().await;
                            }
                        }
                    }
                    Some(Err(error)) => tracing::warn!(%error, "agent event receiver lagged"),
                    None => break,
                },
                command = self.command_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UiCommand::Submit(input) => {
                            if let Err(error) = self.history_store.append(self.thread.id(), &input).await {
                                tracing::warn!(%error, "failed to persist input history");
                            }
                            match self.thread.submit(input).await {
                                Ok(turn) => {
                                    turns.insert(turn.id(), turn.cancellation_token());
                                }
                                Err(error) => {
                                    let _ = self.event_tx.send(EventKind::Error(error.to_string())).await;
                                }
                            }
                        }
                        UiCommand::Cancel => {
                            if let Some(cancel) = active.and_then(|id| turns.get(&id)) {
                                cancel.cancel();
                            }
                        }
                        UiCommand::CancelAndRollback => {
                            if let Some(cancel) = active.and_then(|id| turns.get(&id)) {
                                rollback_after_cancel = true;
                                cancel.cancel();
                            } else {
                                self.rollback_last_turn().await;
                            }
                        }
                        UiCommand::Compact => self.compact_thread().await,
                        UiCommand::NewSession => {
                            self.thread = self.runtime.start(self.agent.clone(), self.options.clone());
                            events = self.thread.events();
                            turns.clear();
                            active = None;
                        }
                        UiCommand::ListSessions => self.list_threads().await,
                        UiCommand::ListForkPoints => self.list_fork_points().await,
                        UiCommand::ResumeSession(thread_id) => {
                            self.resume_thread(thread_id).await;
                            events = self.thread.events();
                            turns.clear();
                            active = None;
                        }
                        UiCommand::ForkSession(message_id) => {
                            self.fork_thread(message_id).await;
                            events = self.thread.events();
                            turns.clear();
                            active = None;
                        }
                        UiCommand::Exit => {
                            if let Some(cancel) = active.and_then(|id| turns.get(&id)) {
                                cancel.cancel();
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn rollback_last_turn(&mut self) {
        let event = match self.thread.rollback().await {
            Ok(Some(prompt)) => {
                if let Err(error) = self.history_store.undo(self.thread.id(), &prompt).await {
                    tracing::warn!(%error, "failed to undo input history entry");
                }
                let context_tokens = self
                    .thread
                    .messages()
                    .await
                    .ok()
                    .and_then(|messages| self.estimated_context_tokens(&messages));
                EventKind::TurnRolledBack {
                    prompt,
                    context_tokens,
                }
            }
            Ok(None) => EventKind::Error("No submitted turn is available to undo.".to_string()),
            Err(error) => EventKind::Error(format!("Failed to undo the last turn: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn compact_thread(&mut self) {
        let event = match self.thread.compact().await {
            Ok(result) => EventKind::ContextCompacted {
                before_tokens: u64::try_from(result.before_tokens).unwrap_or(u64::MAX),
                after_tokens: u64::try_from(result.after_tokens).unwrap_or(u64::MAX),
                dropped_messages: u64::try_from(result.dropped_messages).unwrap_or(u64::MAX),
                automatic: false,
            },
            Err(error) => EventKind::Error(format!("Failed to compact context: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn list_threads(&self) {
        let event = match self.runtime.threads(Some(self.thread.id())).await {
            Ok(threads) => EventKind::ThreadsListed { threads },
            Err(error) => EventKind::Error(format!("Failed to list saved chats: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn list_fork_points(&self) {
        let event = match self.thread.fork_points().await {
            Ok(points) => EventKind::ForkPointsListed { points },
            Err(error) => EventKind::Error(format!("Failed to list fork points: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    /// Estimate the model-context size for a given message history, using the
    /// same estimator the runtime uses when the API does not report usage.
    fn estimated_context_tokens(&self, messages: &[ash_core::Message]) -> Option<u64> {
        let tools = self
            .agent
            .tools
            .iter()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        let tokens = estimate_request_tokens(self.agent.system_prompt.as_deref(), messages, &tools);
        u64::try_from(tokens).ok()
    }

    async fn resume_thread(&mut self, thread_id: ThreadId) {
        let event = match self
            .runtime
            .resume(self.agent.clone(), self.options.clone(), thread_id)
            .await
        {
            Ok(Some(thread)) => {
                self.thread = thread;
                match self.thread.messages().await {
                    Ok(messages) => EventKind::ThreadRestored {
                        model: self.agent.model.as_str().to_string(),
                        protocol: self.runtime.model_backend().to_string(),
                        working_dir: self.options.working_dir.clone(),
                        context_tokens: self.estimated_context_tokens(&messages),
                        messages,
                    },
                    Err(error) => EventKind::Error(format!("Failed to restore chat: {error}")),
                }
            }
            Ok(None) => EventKind::Error("That saved chat is no longer available.".to_string()),
            Err(error) => EventKind::Error(format!("Failed to resume saved chat: {error}")),
        };
        let _ = self.event_tx.send(event).await;
    }

    async fn fork_thread(&mut self, message_id: MessageId) {
        let event = match self.thread.fork_at(message_id).await {
            Ok(Some(forked)) => {
                self.thread = forked.thread;
                EventKind::ThreadForked {
                    model: forked.model,
                    protocol: forked.protocol,
                    working_dir: forked.working_dir,
                    context_tokens: self.estimated_context_tokens(&forked.messages),
                    messages: forked.messages,
                    prompt: forked.prompt,
                }
            }
            Ok(None) => {
                EventKind::Error("That prompt is no longer available to fork from.".to_string())
            }
            Err(error) => EventKind::Error(format!("Failed to fork the current chat: {error}")),
        };
        let _ = self.event_tx.send(event).await;
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
            assert_eq!(parse_protocol(name).unwrap().as_cli_name(), name);
        }
        assert!(parse_protocol("responses").is_err());
    }
}
