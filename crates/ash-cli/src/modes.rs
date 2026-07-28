use anyhow::{Context, Result};
use ash_agent::{
    build_system_prompt, skill_tool, Agent, AgentConfig, AgentSession, MessageHistoryStore, Skill,
    DEFAULT_MAX_INPUT_TOKENS,
};
use ash_core::{CancellationToken, Event, Message, ModelId, Protocol, ProviderConfig, SessionId};
use ash_tui::UiCommand;
use futures::StreamExt;
use owo_colors::OwoColorize;
use secrecy::SecretString;

use crate::Cli;

#[derive(Debug, Eq, PartialEq)]
enum TurnOutcome {
    Completed,
    Rollback,
    Exit,
}

#[derive(Debug, Eq, PartialEq)]
enum ActiveTurnCommand {
    Defer(String),
    Cancel,
    CancelAndRollback,
    Reject,
    Exit,
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = build_config(&cli)?;

    if cli.print {
        run_print(config, cli.prompt).await
    } else {
        run_interactive(config).await
    }
}

fn build_config(cli: &Cli) -> Result<AgentConfig> {
    let protocol_name = cli
        .protocol
        .clone()
        .or_else(|| env_value("ASH_PROTOCOL"))
        .unwrap_or_else(|| "anthropic".into());
    let protocol = match protocol_name.as_str() {
        "anthropic" => Protocol::AnthropicMessages,
        "openai" => Protocol::OpenaiCompletions,
        "openai-responses" => Protocol::OpenaiResponses,
        other => anyhow::bail!("unknown protocol: {other}"),
    };

    let api_key =
        env_value("ASH_API_KEY").context("set ASH_API_KEY in .env or the process environment")?;

    let model = cli.model.clone().or_else(|| env_value("ASH_MODEL"));
    let model = match model {
        Some(model) => model,
        None if matches!(&protocol, Protocol::AnthropicMessages) => {
            "claude-sonnet-4-20250514".into()
        }
        None => anyhow::bail!("set ASH_MODEL in .env or pass --model"),
    };
    let base_url = cli.base_url.clone().or_else(|| env_value("ASH_BASE_URL"));
    let configured_max_input_tokens = match cli.max_input_tokens {
        Some(value) => Some(value),
        None => match env_usize("ASH_MAX_INPUT_TOKENS")? {
            Some(value) => Some(value),
            None => env_usize("ASH_MAX_CONTEXT_TOKENS")?,
        },
    };
    let max_input_tokens = resolve_max_input_tokens(configured_max_input_tokens)?;
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
        active_skill
            .as_ref()
            .and_then(|skill| skill.tools.as_deref()),
    )?;
    tools.push(skill_tool(skills.clone()));

    let mut config = AgentConfig {
        provider: ProviderConfig {
            protocol,
            api_key: SecretString::from(api_key),
            base_url,
        },
        system_prompt: Some(system_prompt),
        model: ModelId::new(model),
        tools,
        max_turns: 100,
        working_dir,
        max_input_tokens,
        max_output_tokens: None,
        max_tool_duration: std::time::Duration::from_secs(120),
        agent_path: "/root".to_string(),
        root_session_id: None,
    };

    if let Some(skill) = active_skill {
        skill.apply_overrides(&mut config);
    }
    ash_orchestrator::install_subagent_tools(&mut config);

    Ok(config)
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

fn resolve_max_input_tokens(configured: Option<usize>) -> Result<usize> {
    let value = configured.unwrap_or(DEFAULT_MAX_INPUT_TOKENS);
    if value == 0 {
        anyhow::bail!("maximum input tokens must be a positive integer");
    }
    Ok(value)
}

async fn run_print(config: AgentConfig, prompt: Option<String>) -> Result<()> {
    let input = match prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf
        }
    };

    let messages = vec![Message::user(&input)];
    let mut stream = Agent::run(config, messages);

    while let Some(event) = stream.next().await {
        match event {
            ash_core::Event::TextDelta(t) => print!("{t}"),
            ash_core::Event::ToolCallStart { name, .. } => {
                eprintln!("{}", format!("[tool: {name}]").cyan());
            }
            ash_core::Event::ToolCallEnd {
                is_error: true,
                output,
                ..
            } => eprintln!("{}", format!("[error: {output}]").red()),
            ash_core::Event::Error(e) => {
                eprintln!("{}", format!("[error: {e}]").red());
            }
            _ => {}
        }
    }

    println!();
    Ok(())
}

async fn run_interactive(config: AgentConfig) -> Result<()> {
    let protocol = config.provider.protocol.as_cli_name().to_string();
    let model = config.model.as_str().to_string();
    let working_dir = config.working_dir.clone();
    let context_limit = Some(u64::try_from(config.max_input_tokens).unwrap_or(u64::MAX));
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<UiCommand>(16);

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
    let mut session = AgentSession::new(config);
    let app = ash_tui::App::new(protocol, model, working_dir)
        .with_context_limit(context_limit)
        .with_input_history(input_history);
    let app_handle = tokio::spawn(async move { app.run(event_rx, command_tx).await });

    run_controller(&mut session, &event_tx, &mut command_rx, &history_store).await;

    drop(event_tx);
    app_handle.await??;
    Ok(())
}

async fn run_controller(
    session: &mut AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<Event>,
    command_rx: &mut tokio::sync::mpsc::Receiver<UiCommand>,
    history_store: &MessageHistoryStore,
) {
    let mut deferred_submission = None;

    'controller: loop {
        let command = match deferred_submission.take() {
            Some(input) => UiCommand::Submit(input),
            None => match command_rx.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            UiCommand::Submit(input) => {
                if let Err(error) = history_store.append(session.id(), &input).await {
                    tracing::warn!(%error, "failed to persist input history");
                }
                match run_active_turn(
                    session,
                    input,
                    event_tx,
                    command_rx,
                    &mut deferred_submission,
                )
                .await
                {
                    TurnOutcome::Completed => {}
                    TurnOutcome::Rollback => {
                        deferred_submission = None;
                        rollback_last_turn(session, event_tx, history_store).await;
                    }
                    TurnOutcome::Exit => break 'controller,
                }
            }
            UiCommand::CancelAndRollback => {
                deferred_submission = None;
                rollback_last_turn(session, event_tx, history_store).await;
            }
            UiCommand::Cancel => {}
            UiCommand::Rollback => {
                deferred_submission = None;
                rollback_last_turn(session, event_tx, history_store).await;
            }
            UiCommand::Compact => {
                deferred_submission = None;
                compact_session(session, event_tx).await;
            }
            UiCommand::NewSession => {
                session.reset();
                deferred_submission = None;
            }
            UiCommand::ListSessions => {
                list_sessions(session, event_tx).await;
            }
            UiCommand::ResumeSession(session_id) => {
                deferred_submission = None;
                resume_session(session, session_id, event_tx).await;
            }
            UiCommand::Exit => break,
        }
    }
}

async fn run_active_turn(
    session: &mut AgentSession,
    input: String,
    event_tx: &tokio::sync::mpsc::Sender<Event>,
    command_rx: &mut tokio::sync::mpsc::Receiver<UiCommand>,
    deferred_submission: &mut Option<String>,
) -> TurnOutcome {
    let cancel = CancellationToken::new();
    let mut turn = Box::pin(session.submit(input, event_tx.clone(), cancel.clone()));
    let outcome = loop {
        tokio::select! {
            _ = &mut turn => break TurnOutcome::Completed,
            command = command_rx.recv() => match classify_active_command(command) {
                ActiveTurnCommand::Defer(input) => *deferred_submission = Some(input),
                ActiveTurnCommand::Reject => {
                    let _ = event_tx
                        .send(Event::Error(
                            "That command is unavailable while working.".to_string(),
                        ))
                        .await;
                }
                ActiveTurnCommand::Cancel => {
                    cancel.cancel();
                    let _ = (&mut turn).await;
                    *deferred_submission = None;
                    break TurnOutcome::Completed;
                }
                ActiveTurnCommand::CancelAndRollback => {
                    cancel.cancel();
                    let _ = (&mut turn).await;
                    break TurnOutcome::Rollback;
                }
                ActiveTurnCommand::Exit => {
                    cancel.cancel();
                    let _ = (&mut turn).await;
                    break TurnOutcome::Exit;
                }
            }
        }
    };
    drop(turn);
    outcome
}

fn classify_active_command(command: Option<UiCommand>) -> ActiveTurnCommand {
    match command {
        Some(UiCommand::Submit(input)) => ActiveTurnCommand::Defer(input),
        Some(UiCommand::Cancel) => ActiveTurnCommand::Cancel,
        Some(UiCommand::CancelAndRollback) => ActiveTurnCommand::CancelAndRollback,
        Some(
            UiCommand::NewSession
            | UiCommand::Rollback
            | UiCommand::Compact
            | UiCommand::ListSessions
            | UiCommand::ResumeSession(_),
        ) => ActiveTurnCommand::Reject,
        Some(UiCommand::Exit) | None => ActiveTurnCommand::Exit,
    }
}

async fn rollback_last_turn(
    session: &mut AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
    history_store: &MessageHistoryStore,
) {
    let event = match session.rollback_last_turn().await {
        Ok(Some(prompt)) => {
            if let Err(error) = history_store.undo(session.id(), &prompt).await {
                tracing::warn!(%error, "failed to undo input history entry");
            }
            ash_core::Event::TurnRolledBack { prompt }
        }
        Ok(None) => ash_core::Event::Error("No submitted turn is available to undo.".to_string()),
        Err(error) => ash_core::Event::Error(format!("Failed to undo the last turn: {error}")),
    };
    let _ = event_tx.send(event).await;
}

async fn compact_session(
    session: &mut AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
) {
    let event = match session.compact().await {
        Ok(result) => ash_core::Event::ContextCompacted {
            before_tokens: u64::try_from(result.before_tokens).unwrap_or(u64::MAX),
            after_tokens: u64::try_from(result.after_tokens).unwrap_or(u64::MAX),
            dropped_messages: u64::try_from(result.dropped_messages).unwrap_or(u64::MAX),
            automatic: false,
        },
        Err(error) => ash_core::Event::Error(format!("Failed to compact context: {error}")),
    };
    let _ = event_tx.send(event).await;
}

async fn list_sessions(
    session: &AgentSession,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
) {
    let event = match session.resumable_sessions().await {
        Ok(sessions) => ash_core::Event::SessionsListed { sessions },
        Err(error) => ash_core::Event::Error(format!("Failed to list saved chats: {error}")),
    };
    let _ = event_tx.send(event).await;
}

async fn resume_session(
    session: &mut AgentSession,
    session_id: SessionId,
    event_tx: &tokio::sync::mpsc::Sender<ash_core::Event>,
) {
    let event = match session.resume(session_id).await {
        Ok(Some(restored)) => ash_core::Event::SessionRestored {
            model: restored.model,
            protocol: restored.protocol,
            working_dir: restored.working_dir,
            messages: restored.messages,
        },
        Ok(None) => ash_core::Event::Error("That saved chat is no longer available.".to_string()),
        Err(error) => ash_core::Event::Error(format!("Failed to resume saved chat: {error}")),
    };
    let _ = event_tx.send(event).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_input_limit_defaults_to_200k() {
        assert_eq!(resolve_max_input_tokens(None).unwrap(), 200_000);
        assert_eq!(resolve_max_input_tokens(Some(64_000)).unwrap(), 64_000);
        assert!(resolve_max_input_tokens(Some(0)).is_err());
    }

    #[test]
    fn active_turn_commands_have_one_explicit_policy() {
        assert_eq!(
            classify_active_command(Some(UiCommand::Submit("next".into()))),
            ActiveTurnCommand::Defer("next".into())
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::Cancel)),
            ActiveTurnCommand::Cancel
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::CancelAndRollback)),
            ActiveTurnCommand::CancelAndRollback
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::NewSession)),
            ActiveTurnCommand::Reject
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::Rollback)),
            ActiveTurnCommand::Reject
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::Compact)),
            ActiveTurnCommand::Reject
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::ListSessions)),
            ActiveTurnCommand::Reject
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::ResumeSession(SessionId::new()))),
            ActiveTurnCommand::Reject
        );
        assert_eq!(
            classify_active_command(Some(UiCommand::Exit)),
            ActiveTurnCommand::Exit
        );
        assert_eq!(classify_active_command(None), ActiveTurnCommand::Exit);
    }
}
