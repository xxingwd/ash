use ash_core::EventKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackgroundAction {
    ListSessions,
    ListForkPoints,
    Resume,
    Fork,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmissionPolicy {
    Start,
    Enqueue,
    Block,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Cancellation {
    KeepResponse,
    RemoveTurn { prompt: Option<String> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnCompletion {
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RollbackCompletion {
    ReplayViewport,
    ViewportAlreadyRemoved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentStart {
    StartedTurn,
    TurnAlreadyTracked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureCompletion {
    FinishedOperation,
    OperationUnchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventRoute {
    Handle,
    Ignore,
    Render,
}

#[derive(Debug, Default)]
pub(crate) struct OperationState {
    current: Operation,
}

#[derive(Debug, Default)]
enum Operation {
    #[default]
    Idle,
    Turn(TurnOperation),
    Background(BackgroundAction),
}

#[derive(Debug)]
enum TurnOperation {
    Running { prompt: Option<String> },
    Cancelling,
    RollingBack(Rollback),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackStage {
    AwaitingTurn,
    AwaitingResult,
}

#[derive(Debug)]
struct Rollback {
    stage: RollbackStage,
    /// Whether the turn was already removed from the viewport when the
    /// rollback started (cancel path) or still needs removal (undo path).
    viewport_removed: bool,
}

impl OperationState {
    pub(crate) fn is_busy(&self) -> bool {
        !matches!(self.current, Operation::Idle)
    }

    pub(crate) fn shows_activity(&self) -> bool {
        match &self.current {
            // A cancel-path rollback is still shutting the running turn down;
            // an undo-path rollback has nothing running to animate.
            Operation::Turn(TurnOperation::RollingBack(rollback)) => rollback.viewport_removed,
            Operation::Turn(_) | Operation::Background(BackgroundAction::Compact) => true,
            Operation::Background(_) | Operation::Idle => false,
        }
    }

    pub(crate) fn submission_policy(&self) -> SubmissionPolicy {
        match self.current {
            Operation::Idle => SubmissionPolicy::Start,
            Operation::Turn(TurnOperation::Running { .. }) => SubmissionPolicy::Enqueue,
            Operation::Turn(TurnOperation::Cancelling | TurnOperation::RollingBack(_))
            | Operation::Background(_) => SubmissionPolicy::Block,
        }
    }

    pub(crate) fn can_cancel(&self) -> bool {
        matches!(self.current, Operation::Turn(TurnOperation::Running { .. }))
    }

    pub(crate) fn start_turn(&mut self, prompt: Option<String>) {
        self.current = Operation::Turn(TurnOperation::Running { prompt });
    }

    pub(crate) fn agent_started(&mut self) -> AgentStart {
        if self.is_busy() {
            AgentStart::TurnAlreadyTracked
        } else {
            self.start_turn(None);
            AgentStart::StartedTurn
        }
    }

    pub(crate) fn start_background(&mut self, action: BackgroundAction) {
        self.current = Operation::Background(action);
    }

    /// Begin a rollback of the last completed turn (`/undo`), where the turn
    /// is still visible and must be removed from the viewport when the
    /// rollback result arrives.
    pub(crate) fn start_rollback(&mut self) {
        self.current = Operation::Turn(TurnOperation::RollingBack(Rollback {
            stage: RollbackStage::AwaitingResult,
            viewport_removed: false,
        }));
    }

    pub(crate) fn begin_cancellation(&mut self, has_response: bool) -> Option<Cancellation> {
        let current = std::mem::take(&mut self.current);
        let Operation::Turn(TurnOperation::Running { prompt }) = current else {
            self.current = current;
            return None;
        };

        if has_response {
            self.current = Operation::Turn(TurnOperation::Cancelling);
            Some(Cancellation::KeepResponse)
        } else {
            self.current = Operation::Turn(TurnOperation::RollingBack(Rollback {
                stage: RollbackStage::AwaitingTurn,
                viewport_removed: true,
            }));
            Some(Cancellation::RemoveTurn { prompt })
        }
    }

    pub(crate) fn route_event(&mut self, event: &EventKind) -> EventRoute {
        match (&mut self.current, event) {
            (Operation::Turn(TurnOperation::RollingBack(rollback)), EventKind::Turn(_)) => {
                rollback.stage = RollbackStage::AwaitingResult;
                EventRoute::Render
            }
            (
                Operation::Turn(TurnOperation::RollingBack(Rollback {
                    stage: RollbackStage::AwaitingTurn,
                    ..
                })),
                EventKind::Error(_),
            ) => EventRoute::Ignore,
            (Operation::Background(_), EventKind::Turn(_)) => EventRoute::Ignore,
            (operation, event)
                if suppresses_turn_output(operation) && is_turn_output_event(event) =>
            {
                EventRoute::Ignore
            }
            _ => EventRoute::Handle,
        }
    }

    pub(crate) fn complete_turn(&mut self) -> Option<TurnCompletion> {
        let current = std::mem::take(&mut self.current);
        match current {
            Operation::Idle | Operation::Turn(TurnOperation::Running { .. }) => {
                Some(TurnCompletion::Completed)
            }
            Operation::Turn(TurnOperation::Cancelling) => Some(TurnCompletion::Cancelled),
            current @ (Operation::Turn(TurnOperation::RollingBack(_))
            | Operation::Background(_)) => {
                self.current = current;
                None
            }
        }
    }

    pub(crate) fn complete_failed_action(&mut self) -> FailureCompletion {
        let is_action_result = matches!(
            self.current,
            Operation::Background(_)
                | Operation::Turn(TurnOperation::RollingBack(Rollback {
                    stage: RollbackStage::AwaitingResult,
                    ..
                }))
        );
        if is_action_result {
            self.finish();
            FailureCompletion::FinishedOperation
        } else {
            FailureCompletion::OperationUnchanged
        }
    }

    pub(crate) fn finish_background(&mut self, action: BackgroundAction) {
        if matches!(self.current, Operation::Background(current) if current == action) {
            self.finish();
        }
    }

    pub(crate) fn finish(&mut self) {
        self.current = Operation::Idle;
    }

    pub(crate) fn finish_rollback(&mut self) -> RollbackCompletion {
        let viewport_removed = matches!(
            self.current,
            Operation::Turn(TurnOperation::RollingBack(Rollback {
                viewport_removed: true,
                ..
            }))
        );
        self.finish();
        if viewport_removed {
            RollbackCompletion::ViewportAlreadyRemoved
        } else {
            RollbackCompletion::ReplayViewport
        }
    }
}

fn suppresses_turn_output(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Background(_)
            | Operation::Turn(TurnOperation::RollingBack(Rollback {
                stage: RollbackStage::AwaitingTurn,
                ..
            }))
    )
}

fn is_turn_output_event(event: &EventKind) -> bool {
    matches!(event, EventKind::TurnStart | EventKind::Live(_))
}

#[cfg(test)]
mod tests {
    use ash_core::{LiveEvent, StopReason, TurnId, TurnResult, TurnView};

    use super::*;

    fn turn_event(reason: StopReason) -> EventKind {
        EventKind::Turn(TurnView {
            id: TurnId::new(),
            result: TurnResult::Completed(reason),
            messages: Vec::new(),
            usage: None,
            context_tokens: None,
        })
    }

    #[test]
    fn agent_start_reports_whether_the_turn_was_already_tracked() {
        let mut state = OperationState::default();

        assert_eq!(state.agent_started(), AgentStart::StartedTurn);
        assert_eq!(state.agent_started(), AgentStart::TurnAlreadyTracked);
    }

    #[test]
    fn turn_errors_do_not_finish_the_operation_before_agent_finished() {
        let mut state = OperationState::default();
        state.start_turn(Some("question".into()));

        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::OperationUnchanged
        );
        assert!(state.is_busy());
    }

    #[test]
    fn submission_policy_follows_the_active_operation() {
        let mut state = OperationState::default();
        assert_eq!(state.submission_policy(), SubmissionPolicy::Start);

        state.start_turn(Some("question".into()));
        assert_eq!(state.submission_policy(), SubmissionPolicy::Enqueue);

        assert_eq!(
            state.begin_cancellation(true),
            Some(Cancellation::KeepResponse)
        );
        assert_eq!(state.submission_policy(), SubmissionPolicy::Block);
    }

    #[test]
    fn rollback_waits_for_the_turn_before_accepting_its_result() {
        let mut state = OperationState::default();
        state.start_turn(Some("question".into()));
        assert_eq!(
            state.begin_cancellation(false),
            Some(Cancellation::RemoveTurn {
                prompt: Some("question".into())
            })
        );

        assert_eq!(
            state.route_event(&EventKind::Live(LiveEvent::TextDelta("late".into()))),
            EventRoute::Ignore
        );
        assert_eq!(
            state.route_event(&EventKind::Error("cancelled".into())),
            EventRoute::Ignore
        );
        assert_eq!(
            state.route_event(&turn_event(StopReason::Aborted)),
            EventRoute::Render
        );
        assert_eq!(
            state.route_event(&EventKind::Live(LiveEvent::TextDelta(
                "rollback result".into()
            ))),
            EventRoute::Handle
        );
        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::FinishedOperation
        );
        assert!(!state.is_busy());
    }

    #[test]
    fn background_actions_hide_late_turn_events() {
        let mut state = OperationState::default();
        state.start_background(BackgroundAction::ListSessions);

        assert!(!state.shows_activity());
        assert_eq!(
            state.route_event(&EventKind::Live(LiveEvent::ReasoningDelta("late".into()))),
            EventRoute::Ignore
        );
        assert_eq!(
            state.route_event(&turn_event(StopReason::EndTurn)),
            EventRoute::Ignore
        );
        assert_eq!(
            state.route_event(&EventKind::Error("list failed".into())),
            EventRoute::Handle
        );
        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::FinishedOperation
        );
    }

    #[test]
    fn cancelled_turn_rollback_records_that_the_viewport_was_already_removed() {
        let mut state = OperationState::default();
        state.start_turn(None);
        state.begin_cancellation(false);
        assert_eq!(
            state.finish_rollback(),
            RollbackCompletion::ViewportAlreadyRemoved
        );
    }

    #[test]
    fn undo_rollback_replays_the_viewport() {
        let mut state = OperationState::default();
        state.start_rollback();
        assert_eq!(state.submission_policy(), SubmissionPolicy::Block);
        assert!(!state.shows_activity());
        assert_eq!(state.finish_rollback(), RollbackCompletion::ReplayViewport);
        assert!(!state.is_busy());
    }

    #[test]
    fn undo_rollback_error_finishes_the_operation() {
        let mut state = OperationState::default();
        state.start_rollback();
        assert_eq!(
            state.route_event(&EventKind::Error("nothing to undo".into())),
            EventRoute::Handle
        );
        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::FinishedOperation
        );
        assert!(!state.is_busy());
    }
}
