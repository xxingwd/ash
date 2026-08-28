use ash_core::TurnId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForkOption {
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
}
