use std::sync::Arc;

use ash_core::{
    CancellationToken, Event, Message, ModelClient, RunId, SessionId, StopReason, TurnId,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    agent::{run_agent_turn_identified, ExecutionIds},
    AgentConfig, AgentInput, AgentSession, ConversationRepository, JsonlConversationRepository,
    SharedConversationRepository,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub sequence: u64,
    pub timestamp: String,
    pub event: Event,
}

pub struct AgentRun {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub events: ReceiverStream<RunEvent>,
    cancellation: CancellationToken,
    completion: oneshot::Receiver<Result<StopReason, ash_core::AshError>>,
}

impl AgentRun {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub async fn wait(mut self) -> Result<StopReason, ash_core::AshError> {
        loop {
            tokio::select! {
                result = &mut self.completion => return completion_result(result),
                event = self.events.next() => {
                    if event.is_none() {
                        return completion_result(self.completion.await);
                    }
                }
            }
        }
    }
}

fn completion_result(
    result: Result<Result<StopReason, ash_core::AshError>, oneshot::error::RecvError>,
) -> Result<StopReason, ash_core::AshError> {
    result.map_err(|_| {
        ash_core::AshError::Config("agent run ended without a completion result".to_string())
    })?
}

/// Shared, provider-neutral dependencies used to create agent sessions.
#[derive(Clone)]
pub struct AgentRuntime {
    model: Arc<dyn ModelClient>,
    model_backend: Arc<str>,
    conversations: SharedConversationRepository,
}

impl AgentRuntime {
    pub fn new(model: Arc<dyn ModelClient>, model_backend: impl Into<String>) -> Self {
        Self {
            model,
            model_backend: Arc::from(model_backend.into()),
            conversations: Arc::new(JsonlConversationRepository::default()),
        }
    }

    pub fn with_conversation_repository(
        mut self,
        conversations: Arc<dyn ConversationRepository>,
    ) -> Self {
        self.conversations = conversations;
        self
    }

    pub fn create_session(&self, config: impl Into<AgentConfig>) -> AgentSession {
        AgentSession::new(config.into(), self.clone())
    }

    pub fn start(
        &self,
        config: impl Into<AgentConfig>,
        history: Vec<Message>,
        input: AgentInput,
    ) -> Result<AgentRun, ash_core::AshError> {
        self.start_in_session(config, history, input, SessionId::new())
    }

    pub fn start_in_session(
        &self,
        config: impl Into<AgentConfig>,
        mut history: Vec<Message>,
        input: AgentInput,
        session_id: SessionId,
    ) -> Result<AgentRun, ash_core::AshError> {
        let config = config.into();
        if input.content.is_empty() {
            return Err(ash_core::AshError::Config(
                "agent input content cannot be empty".to_string(),
            ));
        }
        history.push(input.into_message());
        let run_id = RunId::new();
        let turn_id = TurnId::new();
        let cancellation = CancellationToken::new();
        let (payload_tx, mut payload_rx) = mpsc::channel(64);
        let (event_tx, event_rx) = mpsc::channel(64);
        let (completion_tx, completion) = oneshot::channel();
        let model = self.model_client();
        let run_cancel = cancellation.clone();

        tokio::spawn(async move {
            let result = run_agent_turn_identified(
                model.as_ref(),
                &config,
                &mut history,
                payload_tx,
                run_cancel,
                ExecutionIds {
                    session_id,
                    run_id,
                    turn_id,
                },
            )
            .await;
            let _ = completion_tx.send(result);
        });
        tokio::spawn(async move {
            let mut sequence = 0_u64;
            while let Some(event) = payload_rx.recv().await {
                sequence = sequence.saturating_add(1);
                if event_tx
                    .send(RunEvent {
                        session_id,
                        run_id,
                        turn_id,
                        sequence,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        event,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(AgentRun {
            session_id,
            run_id,
            turn_id,
            events: ReceiverStream::new(event_rx),
            cancellation,
            completion,
        })
    }

    pub fn model_client(&self) -> Arc<dyn ModelClient> {
        Arc::clone(&self.model)
    }

    pub fn model_backend(&self) -> &str {
        &self.model_backend
    }

    pub(crate) fn model(&self) -> &dyn ModelClient {
        self.model.as_ref()
    }

    pub(crate) fn conversations(&self) -> &dyn ConversationRepository {
        self.conversations.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use ash_core::{ModelRequest, ModelStream, ModelStreamEvent, ProtocolError, StopReason};
    use futures::StreamExt;

    use super::*;
    use crate::{CodingContextPolicy, Trigger};

    struct TestModel;

    impl ModelClient for TestModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            Ok(Box::pin(futures::stream::iter([
                Ok(ModelStreamEvent::TextDelta("done".to_string())),
                Ok(ModelStreamEvent::Stop(StopReason::EndTurn)),
            ])))
        }
    }

    struct BurstModel;

    impl ModelClient for BurstModel {
        fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProtocolError> {
            let mut events = (0..200)
                .map(|index| Ok(ModelStreamEvent::TextDelta(index.to_string())))
                .collect::<Vec<_>>();
            events.push(Ok(ModelStreamEvent::Stop(StopReason::EndTurn)));
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn config() -> AgentConfig {
        AgentConfig {
            system_prompt: None,
            tools: Vec::new(),
            model: ash_core::ModelId::new("test"),
            max_turns: 1,
            working_dir: PathBuf::from("."),
            max_context_tokens: 10_000,
            context_policy: Arc::new(CodingContextPolicy),
            max_tool_duration: Duration::from_secs(1),
            agent_path: "/root".to_string(),
            root_session_id: None,
        }
    }

    #[tokio::test]
    async fn run_events_have_stable_identifiers_and_monotonic_sequences() {
        let runtime = AgentRuntime::new(Arc::new(TestModel), "test");
        let mut run = runtime
            .start(
                config(),
                Vec::new(),
                AgentInput::with_trigger(Trigger::Scheduled, "run maintenance"),
            )
            .unwrap();
        let session_id = run.session_id;
        let run_id = run.run_id;
        let turn_id = run.turn_id;
        let mut events = Vec::new();
        while let Some(event) = run.events.next().await {
            events.push(event);
        }

        assert_eq!(run.wait().await.unwrap(), StopReason::EndTurn);
        assert!(!events.is_empty());
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.session_id, session_id);
            assert_eq!(event.run_id, run_id);
            assert_eq!(event.turn_id, turn_id);
            assert_eq!(event.sequence, u64::try_from(index + 1).unwrap());
        }
        assert!(events
            .iter()
            .any(|event| matches!(&event.event, Event::TextDelta(text) if text == "done")));
    }

    #[tokio::test]
    async fn wait_drains_unconsumed_events_without_deadlocking() {
        let runtime = AgentRuntime::new(Arc::new(BurstModel), "test");
        let run = runtime
            .start(config(), Vec::new(), AgentInput::user("run"))
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), run.wait())
            .await
            .expect("run should not depend on an external event consumer")
            .unwrap();

        assert_eq!(result, StopReason::EndTurn);
    }
}
