//! Static decision provenance. No classifier request is implemented or implied.
use super::{PermissionMode, PermissionResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionOrigin {
    ExplicitRule,
    ToolSetting,
    PlanBoundary,
    SavedApproval,
    Mode,
    DefaultRead,
    /// A tool that only changes this app's own session state.
    DefaultOwnState,
    DefaultDelegation,
    DefaultPrompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDecision {
    pub result: PermissionResult,
    pub origin: DecisionOrigin,
    /// Only a default unresolved action is eligible for a future classifier.
    /// Explicit asks and hard denials must never be silently reclassified.
    pub classifier_eligible: bool,
}
impl PermissionDecision {
    pub(super) fn new(result: PermissionResult, origin: DecisionOrigin) -> Self {
        Self {
            classifier_eligible: origin == DecisionOrigin::DefaultPrompt
                && matches!(result, PermissionResult::RequiresConfirmation(_)),
            result,
            origin,
        }
    }
    pub(super) fn in_mode(mut self, mode: PermissionMode) -> Self {
        if mode == PermissionMode::DontAsk
            && matches!(self.result, PermissionResult::RequiresConfirmation(_))
        {
            self.result = PermissionResult::Denied(
                "dontAsk mode: this action requires permission and was not pre-approved".into(),
            );
            self.classifier_eligible = false;
        }
        self
    }
}
