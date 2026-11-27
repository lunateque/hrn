//! Session-scoped `/fast`, `/slow`, and `/extended-context` backend hooks for `hrn chat`.

use super::ChatBackend;
use hrn_kernel::Tier;

impl ChatBackend {
    pub(super) fn supports_service_tier_live(&self, model: &str) -> bool {
        self.runtime.supports_service_tier(Some(model))
    }

    pub(super) fn set_service_tier_live(&self, tier: Option<Tier>) {
        self.background.overrides.set_service_tier(tier);
    }

    pub(super) fn slow_mode_live(&self) -> bool {
        self.background
            .overrides
            .slow_mode()
            .unwrap_or(self.llm.anthropic.slow_mode)
    }

    pub(super) fn set_slow_mode_live(&self, enabled: bool) {
        self.background.overrides.set_slow_mode(Some(enabled));
    }

    pub(super) fn extended_context_live(&self) -> bool {
        self.background
            .overrides
            .extended_context()
            .unwrap_or(self.llm.extended_context)
    }

    pub(super) fn set_extended_context_live(&self, enabled: bool) {
        self.background
            .overrides
            .set_extended_context(Some(enabled));
    }
}
