#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackgroundAction {
    ListSessions,
    ListForkPoints,
    Resume,
    Fork,
    Compact,
    Rollback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmissionPolicy {
    Start,
    Steer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationMode {
    Interrupt,
    Rollback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnCompletion {
    Commit,
    AwaitRollback,
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
    Running,
    Cancelling(CancellationMode),
}

impl OperationState {
    pub(crate) fn is_busy(&self) -> bool {
        !matches!(self.current, Operation::Idle)
    }

    pub(crate) fn shows_activity(&self) -> bool {
        matches!(
            self.current,
            Operation::Turn(_) | Operation::Background(BackgroundAction::Compact)
        )
    }

    pub(crate) fn submission_policy(&self) -> Option<SubmissionPolicy> {
        match self.current {
            Operation::Idle => Some(SubmissionPolicy::Start),
            Operation::Turn(TurnOperation::Running) => Some(SubmissionPolicy::Steer),
            Operation::Turn(TurnOperation::Cancelling(_)) | Operation::Background(_) => None,
        }
    }

    pub(crate) fn can_cancel(&self) -> bool {
        matches!(self.current, Operation::Turn(TurnOperation::Running))
    }

    pub(crate) fn accepts_live_output(&self) -> bool {
        matches!(self.current, Operation::Turn(TurnOperation::Running))
    }

    pub(crate) fn start_turn(&mut self) {
        self.current = Operation::Turn(TurnOperation::Running);
    }

    pub(crate) fn agent_started(&mut self) -> AgentStart {
        if self.is_busy() {
            AgentStart::TurnAlreadyTracked
        } else {
            self.start_turn();
            AgentStart::StartedTurn
        }
    }

    pub(crate) fn start_background(&mut self, action: BackgroundAction) {
        self.current = Operation::Background(action);
    }

    pub(crate) fn begin_cancellation(&mut self, mode: CancellationMode) -> bool {
        if !matches!(self.current, Operation::Turn(TurnOperation::Running)) {
            return false;
        }
        self.current = Operation::Turn(TurnOperation::Cancelling(mode));
        true
    }

    pub(crate) fn complete_turn(&mut self) -> TurnCompletion {
        let current = std::mem::take(&mut self.current);
        match current {
            Operation::Turn(TurnOperation::Cancelling(CancellationMode::Rollback)) => {
                self.current = Operation::Background(BackgroundAction::Rollback);
                TurnCompletion::AwaitRollback
            }
            current @ Operation::Background(_) => {
                self.current = current;
                TurnCompletion::Commit
            }
            Operation::Idle
            | Operation::Turn(TurnOperation::Running)
            | Operation::Turn(TurnOperation::Cancelling(CancellationMode::Interrupt)) => {
                TurnCompletion::Commit
            }
        }
    }

    pub(crate) fn complete_failed_action(&mut self) -> FailureCompletion {
        if matches!(self.current, Operation::Background(_)) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_start_reports_whether_the_turn_was_already_tracked() {
        let mut state = OperationState::default();

        assert_eq!(state.agent_started(), AgentStart::StartedTurn);
        assert_eq!(state.agent_started(), AgentStart::TurnAlreadyTracked);
    }

    #[test]
    fn turn_errors_do_not_finish_the_operation_before_agent_finished() {
        let mut state = OperationState::default();
        state.start_turn();

        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::OperationUnchanged
        );
        assert!(state.is_busy());
    }

    #[test]
    fn submission_policy_follows_the_active_operation() {
        let mut state = OperationState::default();
        assert_eq!(state.submission_policy(), Some(SubmissionPolicy::Start));

        state.start_turn();
        assert_eq!(state.submission_policy(), Some(SubmissionPolicy::Steer));

        assert!(state.begin_cancellation(CancellationMode::Interrupt));
        assert_eq!(state.submission_policy(), None);
    }

    #[test]
    fn cancellation_keeps_late_output_until_the_turn_finishes() {
        let mut state = OperationState::default();
        state.start_turn();
        assert!(state.begin_cancellation(CancellationMode::Interrupt));

        assert_eq!(state.complete_turn(), TurnCompletion::Commit);
        assert!(!state.is_busy());
    }

    #[test]
    fn cancellation_without_a_completed_tool_becomes_a_rollback() {
        let mut state = OperationState::default();
        state.start_turn();

        assert!(state.begin_cancellation(CancellationMode::Rollback));
        assert!(!state.accepts_live_output());

        assert_eq!(state.complete_turn(), TurnCompletion::AwaitRollback);
        assert!(matches!(
            state.current,
            Operation::Background(BackgroundAction::Rollback)
        ));
    }

    #[test]
    fn turn_completion_preserves_the_background_action() {
        let mut state = OperationState::default();
        state.start_background(BackgroundAction::ListSessions);

        assert!(!state.shows_activity());
        assert_eq!(state.complete_turn(), TurnCompletion::Commit);
        assert!(state.is_busy());
        assert_eq!(
            state.complete_failed_action(),
            FailureCompletion::FinishedOperation
        );
    }

    #[test]
    fn rollback_is_a_background_action() {
        let mut state = OperationState::default();
        state.start_background(BackgroundAction::Rollback);

        assert_eq!(state.submission_policy(), None);
        assert!(!state.shows_activity());

        state.finish_background(BackgroundAction::Rollback);
        assert!(!state.is_busy());
    }
}
