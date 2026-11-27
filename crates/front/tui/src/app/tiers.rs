//! Session-scoped `/fast`, `/slow`, and `/extended-context` slash command handlers.

use super::App;
use crate::transcript::Level;
use hrn_kernel::Tier;

/// Exact user-facing error when `/fast` is requested on a model without service-tier support.
pub(crate) const FAST_UNAVAILABLE_MSG: &str = "Fast mode is unavailable for the current model.";

impl App {
    /// `/fast [ultra|off]`: toggle session priority/ultrafast service tier.
    pub(super) fn handle_fast(&mut self, raw: Option<&str>) {
        let target = match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => match self.settings.service_tier {
                Tier::Priority | Tier::Ultrafast => Tier::None,
                _ => Tier::Priority,
            },
            Some(arg) if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("none") => {
                Tier::None
            }
            Some(arg)
                if arg.eq_ignore_ascii_case("on")
                    || arg.eq_ignore_ascii_case("fast")
                    || arg.eq_ignore_ascii_case("priority") =>
            {
                Tier::Priority
            }
            Some(arg)
                if arg.eq_ignore_ascii_case("ultra") || arg.eq_ignore_ascii_case("ultrafast") =>
            {
                Tier::Ultrafast
            }
            Some(_) => {
                self.notice(Level::Info, "usage: /fast [ultra|off]");
                return;
            }
        };

        if target != Tier::None && !self.backend.supports_service_tier(&self.settings.model) {
            self.notice(Level::Error, FAST_UNAVAILABLE_MSG);
            return;
        }
        self.settings.service_tier = target;
        self.backend.set_service_tier(Some(target));
        self.backend.apply_settings(&self.settings);
        let label = match target {
            Tier::Priority => "on (priority)",
            Tier::Ultrafast => "ultra (ultrafast)",
            _ => "off",
        };
        self.flash(format!("fast mode → {label}"));
    }

    /// `/slow [on|off]`: toggle Anthropic subscription low-priority recovery lane.
    pub(super) fn handle_slow(&mut self, raw: Option<&str>) {
        let next = match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => !self.backend.slow_mode(),
            Some(arg) if arg.eq_ignore_ascii_case("on") || arg.eq_ignore_ascii_case("true") => true,
            Some(arg) if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("false") => {
                false
            }
            Some(_) => {
                self.notice(Level::Info, "usage: /slow [on|off]");
                return;
            }
        };
        self.backend.set_slow_mode(next);
        self.flash(format!("slow mode → {}", if next { "on" } else { "off" }));
    }

    /// `/extended-context [on|off|status]`: switch between base and `max_context_window`.
    pub(super) fn handle_extended_context(&mut self, raw: Option<&str>) {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => {
                let on = self.backend.extended_context();
                self.notice(
                    Level::Info,
                    format!(
                        "extended context: {} (window: {} tokens)",
                        if on { "on" } else { "off" },
                        self.context_window
                    ),
                );
            }
            Some(arg) if arg.eq_ignore_ascii_case("status") => {
                let on = self.backend.extended_context();
                self.notice(
                    Level::Info,
                    format!(
                        "extended context: {} (window: {} tokens)",
                        if on { "on" } else { "off" },
                        self.context_window
                    ),
                );
            }
            Some(arg) if arg.eq_ignore_ascii_case("on") || arg.eq_ignore_ascii_case("true") => {
                self.backend.set_extended_context(true);
                self.context_window = self.backend.context_window(&self.settings.model);
                self.flash(format!(
                    "extended context → on ({} tokens)",
                    self.context_window
                ));
            }
            Some(arg) if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("false") => {
                self.backend.set_extended_context(false);
                self.context_window = self.backend.context_window(&self.settings.model);
                self.flash(format!(
                    "extended context → off ({} tokens)",
                    self.context_window
                ));
            }
            Some(_) => self.notice(Level::Info, "usage: /extended-context [on|off|status]"),
        }
    }
}
