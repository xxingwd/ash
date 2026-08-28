#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActivityView {
    #[default]
    Idle,
    Active {
        header: &'static str,
        interruptible: bool,
    },
}

impl ActivityView {
    pub(crate) const fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundAction {
    ListSessions,
    Resume,
    Fork,
    Compact,
    Undo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionPolicy {
    Start,
    Queue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnStartOutcome {
    StartedTurn,
    TurnAlreadyTracked,
}

#[derive(Debug, Default)]
pub struct OperationState {
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
    Cancelling,
}

impl OperationState {
    pub(crate) const fn is_busy(&self) -> bool {
        !matches!(self.current, Operation::Idle)
    }

    pub(crate) const fn shows_activity(&self) -> bool {
        self.activity_view().is_active()
    }

    pub(crate) const fn submission_policy(&self) -> Option<SubmissionPolicy> {
        match self.current {
            Operation::Idle => Some(SubmissionPolicy::Start),
            Operation::Turn(TurnOperation::Running) => Some(SubmissionPolicy::Queue),
            Operation::Turn(TurnOperation::Cancelling) | Operation::Background(_) => None,
        }
    }

    pub(crate) const fn can_cancel(&self) -> bool {
        matches!(
            self.activity_view(),
            ActivityView::Active {
                interruptible: true,
                ..
            }
        )
    }

    pub(crate) const fn activity_view(&self) -> ActivityView {
        match self.current {
            Operation::Turn(TurnOperation::Running) => ActivityView::Active {
                header: "Working",
                interruptible: true,
            },
            Operation::Turn(TurnOperation::Cancelling) => ActivityView::Active {
                header: "Interrupting",
                interruptible: false,
            },
            Operation::Background(BackgroundAction::Compact) => ActivityView::Active {
                header: "Compacting",
                interruptible: false,
            },
            Operation::Idle | Operation::Background(_) => ActivityView::Idle,
        }
    }

    pub(crate) const fn accepts_live_output(&self) -> bool {
        matches!(self.current, Operation::Turn(TurnOperation::Running))
    }

    pub(crate) const fn start_turn(&mut self) {
        self.current = Operation::Turn(TurnOperation::Running);
    }

    pub(crate) const fn turn_started(&mut self) -> TurnStartOutcome {
        if self.is_busy() {
            TurnStartOutcome::TurnAlreadyTracked
        } else {
            self.start_turn();
            TurnStartOutcome::StartedTurn
        }
    }

    pub(crate) const fn start_background(&mut self, action: BackgroundAction) {
        self.current = Operation::Background(action);
    }

    pub(crate) const fn begin_cancellation(&mut self) -> bool {
        if !matches!(self.current, Operation::Turn(TurnOperation::Running)) {
            return false;
        }
        self.current = Operation::Turn(TurnOperation::Cancelling);
        true
    }

    pub(crate) fn complete_turn(&mut self) {
        if matches!(self.current, Operation::Turn(_)) {
            self.current = Operation::Idle;
        }
    }

    pub(crate) fn finish_background(&mut self, action: BackgroundAction) {
        if matches!(self.current, Operation::Background(current) if current == action) {
            self.finish();
        }
    }

    pub(crate) const fn finish(&mut self) {
        self.current = Operation::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_header_follows_the_active_operation() {
        let mut state = OperationState::default();
        assert_eq!(state.activity_view(), ActivityView::Idle);

        state.start_turn();
        assert_eq!(
            state.activity_view(),
            ActivityView::Active {
                header: "Working",
                interruptible: true,
            }
        );
        assert!(state.can_cancel());

        assert!(state.begin_cancellation());
        assert_eq!(
            state.activity_view(),
            ActivityView::Active {
                header: "Interrupting",
                interruptible: false,
            }
        );
        assert!(!state.can_cancel());

        let mut state = OperationState::default();
        state.start_background(BackgroundAction::Compact);
        assert_eq!(
            state.activity_view(),
            ActivityView::Active {
                header: "Compacting",
                interruptible: false,
            }
        );
        assert!(!state.can_cancel());
    }

    #[test]
    fn agent_start_reports_whether_the_turn_was_already_tracked() {
        let mut state = OperationState::default();

        assert_eq!(state.turn_started(), TurnStartOutcome::StartedTurn);
        assert_eq!(state.turn_started(), TurnStartOutcome::TurnAlreadyTracked);
    }

    #[test]
    fn submission_policy_follows_the_active_operation() {
        let mut state = OperationState::default();
        assert_eq!(state.submission_policy(), Some(SubmissionPolicy::Start));

        state.start_turn();
        assert_eq!(state.submission_policy(), Some(SubmissionPolicy::Queue));

        assert!(state.begin_cancellation());
        assert_eq!(state.submission_policy(), None);
    }

    #[test]
    fn cancellation_keeps_late_output_until_the_turn_finishes() {
        let mut state = OperationState::default();
        state.start_turn();
        assert!(state.begin_cancellation());

        state.complete_turn();
        assert!(!state.is_busy());
    }

    #[test]
    fn cancelled_turns_settle_before_the_controller_decides() {
        let mut state = OperationState::default();
        state.start_turn();

        assert!(state.begin_cancellation());
        assert!(!state.accepts_live_output());

        state.complete_turn();
        assert!(!state.is_busy());
    }

    #[test]
    fn turn_completion_preserves_the_background_action() {
        let mut state = OperationState::default();
        state.start_background(BackgroundAction::ListSessions);

        assert!(!state.shows_activity());
        state.complete_turn();
        assert!(state.is_busy());
        state.finish();
        assert!(!state.is_busy());
    }

    #[test]
    fn undo_is_a_background_action() {
        let mut state = OperationState::default();
        state.start_background(BackgroundAction::Undo);

        assert_eq!(state.submission_policy(), None);
        assert!(!state.shows_activity());

        state.finish_background(BackgroundAction::Undo);
        assert!(!state.is_busy());
    }
}
