//! Rules: discovery/matching, the injection hook and the `rule://` URL scheme.

mod book;
mod hook;
mod scheme;

pub use book::{Rule, Rulebook, rule_matches_path, touched_paths};
pub use hook::RulesHook;
pub use scheme::{RuleResolver, RuleScheme};
